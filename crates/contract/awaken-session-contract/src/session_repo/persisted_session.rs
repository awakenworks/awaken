//! The durable Managed Session aggregate and its root-owned projections.

use super::*;

/// The durable, adapter-side configuration of one Managed session, keyed by its
/// id (which is also its thread id). Everything here is what the wire `Session`
/// object needs beyond the runtime's committed transcript.
#[derive(Clone, Debug, PartialEq, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PersistedSession {
    pub session_id: String,
    /// The one optimistic-concurrency fence for baseline, Resource, MCP,
    /// environment, execution, and disposition mutations. New, not-yet-inserted
    /// values use 0.
    pub revision: SessionRevision,
    /// The only immutable configuration authority. A preparation intent is
    /// consumed exactly once and replaced by its frozen baseline.
    pub baseline: crate::SessionBaselineState,
    pub title: Option<String>,
    pub metadata: BTreeMap<String, String>,
    /// Exact durable neutral mutable tool policy. Empty is an intentional clear;
    /// public protocol tool unions are projections and never persistence truth.
    pub tools: crate::SessionToolConfiguration,
    /// All accepted Session Event batches in root-revision order. Entries remain
    /// after processing as the sole durable inbound DTO provenance; User/System
    /// completion remains owned by Dispatch/Thread and Outcome state by the
    /// Thread Outcome aggregate.
    pub event_batches: Vec<crate::SessionEventBatch>,
    /// Monotonic root-CAS environment fence for overlapping driving events.
    /// Execution and disposition remain the durable logical state; completion
    /// membership is owned by `active_activity_epochs`, while this scalar never
    /// rewinds and therefore keeps environment operations uniquely ordered.
    pub activity_epoch: u64,
    /// Epochs of driving activities that have been admitted but have not yet
    /// settled. This is Session activity truth, not a child/coordinator
    /// relationship registry. `activity_epoch` remains the monotonic
    /// environment fence; this set only prevents an out-of-order completion
    /// from closing the shared Running interval while an older activity remains.
    ///
    /// Historical Running rows deserialize with an empty set. A direct settle
    /// of their current scalar epoch is treated as the one legacy activity; a
    /// new admission supersedes that unknowable crash-orphan and starts the
    /// explicit set at its successor epoch.
    pub active_activity_epochs: BTreeSet<u64>,
    /// One continuous authoritative Running interval. It is persisted in the
    /// aggregate so process recovery and overlapping driving events cannot
    /// fabricate gaps or emit two customer-usage intervals.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub running_interval: Option<crate::SessionRuntimeIntervalStart>,
    /// Every closed aggregate Running interval in root-revision order. This is
    /// intentionally retained for the Session lifetime: the public Events API
    /// accepts any prior event id as a page cursor, so truncating this prefix
    /// would make a valid cross-replica cursor unknowable.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub closed_runtime_intervals: Vec<crate::SessionRuntimeInterval>,
    /// Cumulative wall-clock milliseconds across closed Running intervals.
    /// Overlapping activities share one interval, so this is the authoritative
    /// non-double-counted Session runtime quantity used by list-cost pricing.
    pub runtime_active_millis: u64,
    /// Latest cumulative neutral Runtime usage observed by the Session root.
    /// Runtime remains the counter authority; retaining this root-CAS projection
    /// lets no-budget Sessions close an exact historical usage event too.
    #[serde(default)]
    pub usage_cursor: crate::ManagedBudgetUsageCursor,
    /// Exact Managed list-cost budget and immutable price snapshot. All
    /// Session threads share this root-owned admission and settlement fence.
    pub budget: crate::SessionBudgetState,
    /// Durable, secret-free execution-environment phase. Opaque bindings are
    /// interpreted only by the runtime that produced them; this aggregate owns
    /// their transition, not their substrate meaning.
    pub environment: crate::SessionEnvironmentState,
    /// The only initial and hot MCP desired-state authority.
    pub mcp: crate::SessionMcpAttachmentSet,
    /// Durable resource activation state. Its `active` manifest is the exact,
    /// secret-free Session pin; `pending` and activation records make external
    /// realization/release recoverable without importing authorization concepts.
    pub resources: crate::SessionResourceState,
    /// Continuing Session projection ownership; no process-local slot is an
    /// authority for this lease.
    pub realization: Option<SessionRealizationLease>,
    /// Initial Environment realization retry state. Kept inside the Session
    /// root so a reclaimed Worker claim cannot reset the failure budget.
    pub realization_progress: crate::SessionRealizationProgress,
    /// The only durable execution-state authority. The retained serialized key
    /// keeps historical aggregate JSON readable through the store codec.
    #[serde(rename = "status")]
    pub execution: SessionExecutionState,
    /// Retention/public-visibility is independent from execution progress.
    pub disposition: SessionDisposition,
    /// Durable intent/receipt state for terminal Runtime effects. Resource,
    /// Environment, artifact, and process cleanup project from this one fact.
    #[serde(deserialize_with = "crate::terminal_cleanup::deserialize_persisted_operation")]
    pub terminal_cleanup: crate::SessionCleanupOperation,
}

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

    /// Report whether a historical one-stage row already contains its complete
    /// canonical receipt set. The evidence remains inside this aggregate; no
    /// legacy Runtime authoring value is returned to an adapter.
    pub fn has_complete_legacy_terminal_cleanup_evidence(
        &self,
    ) -> Result<bool, crate::SessionCleanupError> {
        self.terminal_cleanup
            .has_complete_legacy_receipts(&self.session_id)
    }

    /// Normalize one complete historical receipt set and retire the Resource
    /// projection atomically. Current callers cannot supply or manufacture
    /// one-stage evidence through this compatibility boundary.
    pub fn normalize_legacy_terminal_cleanup(
        &mut self,
        release_reason: impl Into<String>,
    ) -> Result<bool, crate::SessionCleanupError> {
        if matches!(
            self.environment,
            crate::SessionEnvironmentState::Restoring { .. }
        ) {
            // Historical one-stage evidence predates the exact physical target
            // carried by Restoring. It cannot prove that unpublished target was
            // disposed, so retain both authorities for the current two-stage
            // reconciler instead of normalizing target-free completion bytes.
            return Err(crate::SessionCleanupError::ReceiptMismatch);
        }
        let changed = self
            .terminal_cleanup
            .normalize_legacy_completion(&self.session_id)?;
        if changed {
            self.resources.complete_terminal_release(release_reason);
            self.environment = crate::SessionEnvironmentState::Unmaterialized;
        }
        Ok(changed)
    }

    /// Admit one exact source-dependent preparation under the current
    /// realization lease. The last required receipt atomically changes the
    /// cleanup operation to Disposing, but does not retire Resources or the
    /// Environment before the separate physical receipt is admitted.
    pub fn record_terminal_cleanup_preparation(
        &mut self,
        workspace_id: &str,
        lease: &crate::SessionRealizationLease,
        preparation: crate::SessionCleanupPreparation,
        repository_preparation: Option<crate::SessionCleanupRepositoryPreparation>,
    ) -> Result<bool, crate::SessionCleanupError> {
        if preparation.effect.command.session_id != self.session_id {
            return Err(crate::SessionCleanupError::OperationMismatch);
        }
        if lease != &preparation.effect.lease {
            return Err(crate::SessionCleanupError::RealizationMismatch);
        }
        let expected_restore_target = (preparation.effect.command.thread_id == self.session_id)
            .then(|| {
                self.environment
                    .restoring_request(workspace_id, &self.session_id)
            })
            .flatten();
        if preparation.effect.command.restore_target != expected_restore_target {
            return Err(crate::SessionCleanupError::PreparationReceiptMismatch);
        }
        let Some(current) = self.realization.as_ref() else {
            return Err(crate::SessionCleanupError::RealizationMismatch);
        };
        if !crate::realization_lease_generation_authorizes(current, lease) {
            return Err(crate::SessionCleanupError::RealizationMismatch);
        }
        let current_effect_fence = current
            .sandbox_effect_fence(preparation.effect.command.effect_id.clone())
            .map_err(|_| crate::SessionCleanupError::RealizationMismatch)?;
        if !preparation
            .provider_prepared_effect_fence()
            .authorizes_effect_successor(&current_effect_fence)
        {
            return Err(crate::SessionCleanupError::RealizationMismatch);
        }
        if let Some(repository_preparation) = &repository_preparation {
            repository_preparation.verify_for_resources(
                &self.session_id,
                workspace_id,
                &self.resources,
            )?;
        }
        self.terminal_cleanup.record_preparation(
            &self.session_id,
            preparation,
            repository_preparation,
        )
    }

    /// Join terminal preparation progress with the frozen Environment's one
    /// provider predecessor. A continuation already in `Disposing` remains the
    /// sole owner of A/fingerprint; an ordinary terminal cleanup derives A from
    /// its root Runtime preparation. No provider fact is copied into the
    /// terminal cleanup state.
    fn terminal_provider_disposal_preparation(
        &self,
    ) -> Result<
        Option<awaken_provisioning_contract::SandboxDisposalPreparation>,
        crate::SessionCleanupError,
    > {
        let Some(ordinary) = self
            .terminal_cleanup
            .terminal_provider_disposal_preparation(&self.session_id)?
        else {
            return Ok(None);
        };
        self.environment
            .terminal_disposal_preparation()
            .map_err(|_| crate::SessionCleanupError::ProviderDisposalPreparationMismatch)
            .map(|inherited| inherited.or(Some(ordinary)))
    }

    /// Project one closed terminal action from the complete Session root. The
    /// cleanup operation owns phase/progress; this aggregate join alone chooses
    /// the provider predecessor required by a physical disposal.
    pub fn terminal_cleanup_work_action(
        &self,
    ) -> Result<Option<crate::SessionTerminalCleanupAction>, crate::SessionCleanupError> {
        let provider_disposal = self.terminal_provider_disposal_preparation()?;
        self.terminal_cleanup
            .terminal_work_action(&self.session_id, provider_disposal.as_ref())
    }

    /// Admit the one exact physical-disposal receipt and retire the
    /// Resource/Environment projection in the same aggregate mutation.
    pub fn record_terminal_cleanup_disposal(
        &mut self,
        workspace_id: &str,
        lease: &crate::SessionRealizationLease,
        receipt: crate::SessionCleanupDisposalReceipt,
        release_reason: impl Into<String>,
    ) -> Result<bool, crate::SessionCleanupError> {
        if self.verified_terminal_cleanup()?.is_completed() {
            // The final root CAS may have committed before its response was
            // lost. Completed retains the canonical receipt fingerprint, so an
            // exact no-mutation replay is verifiable without a current lease,
            // Environment, or Resource projection.
            return self
                .terminal_cleanup
                .record_disposal(&self.session_id, None, receipt);
        }
        if !self
            .realization
            .as_ref()
            .is_some_and(|current| crate::realization_lease_generation_authorizes(current, lease))
        {
            return Err(crate::SessionCleanupError::RealizationMismatch);
        }
        let provider_disposal = self
            .terminal_provider_disposal_preparation()?
            .ok_or(crate::SessionCleanupError::DisposalNotReady)?;
        let command = self
            .terminal_cleanup
            .disposal_command(&self.session_id, Some(&provider_disposal))?
            .ok_or(crate::SessionCleanupError::DisposalNotReady)?;
        self.authorize_terminal_cleanup_disposal_effect(
            workspace_id,
            &crate::SessionTerminalCleanupDisposalEffect::new(command, lease.clone()),
        )?;
        let changed = self.terminal_cleanup.record_disposal(
            &self.session_id,
            Some(&provider_disposal),
            receipt,
        )?;
        if changed {
            self.resources.complete_terminal_release(release_reason);
            self.environment = crate::SessionEnvironmentState::Unmaterialized;
        }
        Ok(changed)
    }

    /// Re-derive one source-dependent preparation authorization from the
    /// aggregate's sole operation and realization generation. Physical
    /// deletion is authorized only by
    /// [`Self::authorize_terminal_cleanup_disposal_effect`].
    pub fn authorize_terminal_cleanup_effect(
        &self,
        effect: &crate::SessionTerminalCleanupEffect,
    ) -> Result<
        Option<awaken_provisioning_contract::SandboxDisposalPreparation>,
        crate::SessionCleanupError,
    > {
        if effect.command.session_id != self.session_id {
            return Err(crate::SessionCleanupError::OperationMismatch);
        }
        if !self.realization.as_ref().is_some_and(|current| {
            crate::realization_lease_generation_authorizes(current, &effect.lease)
        }) {
            return Err(crate::SessionCleanupError::RealizationMismatch);
        }
        if !self
            .terminal_cleanup
            .pending_preparation_commands(&self.session_id)?
            .contains(&effect.command.without_restore_target())
        {
            return Err(crate::SessionCleanupError::CommandNotPending);
        }
        if effect.command.thread_id != self.session_id {
            return Ok(None);
        }
        self.environment
            .terminal_disposal_preparation()
            .map_err(|_| crate::SessionCleanupError::ProviderDisposalPreparationMismatch)
    }

    /// Re-derive the one root-owned physical disposal after every exact
    /// preparation receipt is already durable in this aggregate.
    pub fn authorize_terminal_cleanup_disposal_effect(
        &self,
        workspace_id: &str,
        effect: &crate::SessionTerminalCleanupDisposalEffect,
    ) -> Result<(), crate::SessionCleanupError> {
        if effect.command.session_id != self.session_id {
            return Err(crate::SessionCleanupError::OperationMismatch);
        }
        if !self.realization.as_ref().is_some_and(|current| {
            crate::realization_lease_generation_authorizes(current, &effect.lease)
        }) {
            return Err(crate::SessionCleanupError::RealizationMismatch);
        }
        let repository_preparation = self
            .terminal_cleanup
            .repository_preparation()
            .ok_or(crate::SessionCleanupError::DisposalNotReady)?;
        repository_preparation.verify_for_resources(
            &self.session_id,
            workspace_id,
            &self.resources,
        )?;
        effect
            .sandbox_disposal_authorization()
            .map_err(|_| crate::SessionCleanupError::ProviderDisposalPreparationMismatch)?;
        let provider_disposal = self
            .terminal_provider_disposal_preparation()?
            .ok_or(crate::SessionCleanupError::DisposalNotReady)?;
        if self
            .terminal_cleanup
            .disposal_command(&self.session_id, Some(&provider_disposal))?
            .as_ref()
            != Some(&effect.command)
        {
            return Err(crate::SessionCleanupError::CommandNotPending);
        }
        Ok(())
    }

    /// Re-derive the narrow Artifact publication fence that precedes one
    /// checkpoint source release. The Environment state and current realization
    /// remain the only authority: a durable checkpoint in any other phase, a
    /// stale operation, a legacy unfenced operation, or terminal takeover all
    /// reject without granting Resource I/O.
    pub fn authorize_checkpoint_release_artifact_effect(
        &self,
        operation: &crate::SessionEnvironmentOperation,
    ) -> Result<(), crate::SessionEnvironmentReceiptError> {
        if self.is_terminal() {
            return Err(crate::SessionEnvironmentReceiptError::WrongPhase);
        }
        let crate::SessionEnvironmentState::Suspending {
            operation: current_operation,
            suspend_phase: crate::SuspendPhase::ReadyToDispose,
            checkpoint: Some(_),
            ..
        } = &self.environment
        else {
            return Err(crate::SessionEnvironmentReceiptError::WrongPhase);
        };
        if current_operation != operation {
            return Err(crate::SessionEnvironmentReceiptError::Mismatch);
        }
        let (Some(current_lease), Some(asserted_lease)) =
            (self.realization.as_ref(), operation.realization.as_ref())
        else {
            return Err(crate::SessionEnvironmentReceiptError::RealizationStale);
        };
        if !crate::realization_lease_generation_authorizes(current_lease, asserted_lease) {
            return Err(crate::SessionEnvironmentReceiptError::RealizationStale);
        }
        Ok(())
    }

    /// Project the one live source-release preparation effect from complete
    /// root truth. The Environment fixes the canonical suspend operation while
    /// the root contributes the exact current renewable realization lease.
    pub fn source_release_preparation_effect(
        &self,
    ) -> Result<crate::SourceReleasePreparationEffect, crate::SessionEnvironmentReceiptError> {
        if self.is_terminal() {
            return Err(crate::SessionEnvironmentReceiptError::WrongPhase);
        }
        let current_realization = self
            .realization
            .as_ref()
            .ok_or(crate::SessionEnvironmentReceiptError::RealizationStale)?;
        self.environment
            .source_release_preparation_effect(current_realization)
    }

    /// Admit one exact preparation receipt under the same aggregate-current
    /// realization generation that authorized it. A monotonic expiry renewal
    /// may race the root CAS; owner/incarnation/epoch replacement may not.
    pub fn record_source_release_prepared(
        &mut self,
        receipt: &crate::SourceReleasePreparedReceipt,
    ) -> Result<bool, crate::SessionEnvironmentReceiptError> {
        if self.is_terminal() {
            return Err(crate::SessionEnvironmentReceiptError::WrongPhase);
        }
        let current_realization = self
            .realization
            .as_ref()
            .ok_or(crate::SessionEnvironmentReceiptError::RealizationStale)?;
        if !crate::realization_lease_generation_authorizes(
            current_realization,
            receipt.preparation().lease(),
        ) {
            return Err(crate::SessionEnvironmentReceiptError::RealizationStale);
        }
        let current_effect_fence = current_realization
            .sandbox_effect_fence(receipt.preparation().operation().effect_id.as_str())
            .map_err(|_| crate::SessionEnvironmentReceiptError::RealizationStale)?;
        if !receipt
            .provider_prepared_effect_fence()
            .authorizes_effect_successor(&current_effect_fence)
        {
            return Err(crate::SessionEnvironmentReceiptError::RealizationStale);
        }
        self.environment
            .record_source_release_prepared(receipt)
            .map_err(|error| match error {
                crate::SessionEnvironmentTransitionError::ReceiptMismatch => {
                    crate::SessionEnvironmentReceiptError::Mismatch
                }
                crate::SessionEnvironmentTransitionError::NotSuspending
                | crate::SessionEnvironmentTransitionError::WrongPhase => {
                    crate::SessionEnvironmentReceiptError::WrongPhase
                }
                _ => crate::SessionEnvironmentReceiptError::InvalidTransition,
            })
    }

    /// Project the one destructive checkpoint-source effect from the complete
    /// aggregate truth. The Environment owns the durable preparation identity;
    /// the root supplies its current realization so a higher-epoch Worker may
    /// resume Disposing without inheriting source-dependent preparation work.
    pub fn source_release_disposal(
        &self,
    ) -> Result<crate::SourceReleaseDisposal, crate::SessionEnvironmentReceiptError> {
        if self.is_terminal() {
            return Err(crate::SessionEnvironmentReceiptError::WrongPhase);
        }
        let current_realization = self
            .realization
            .as_ref()
            .ok_or(crate::SessionEnvironmentReceiptError::RealizationStale)?;
        self.environment
            .source_release_disposal(current_realization)
    }

    /// Admit one terminal Memory intent from the current aggregate truth.
    /// The caller-supplied evidence is never authoritative: this method decodes
    /// the exact Environment binding stored by the Session and requires one
    /// equality match in both the active frozen manifest and that handle.
    pub fn authorize_terminal_memory_intent(
        &self,
        intent: &crate::SessionTerminalMemoryIntent,
    ) -> Result<(), crate::SessionMemoryReconciliationError> {
        self.authorize_terminal_cleanup_effect(intent.effect())?;
        if self.resources.pending.is_some()
            || self
                .resources
                .active
                .inputs()
                .iter()
                .filter(|input| intent.matches_input(input))
                .count()
                != 1
        {
            return Err(crate::SessionMemoryReconciliationError::ResourceMismatch);
        }

        let binding = self
            .environment
            .binding()
            .ok_or(crate::SessionMemoryReconciliationError::EnvironmentMismatch)?;
        let handle = serde_json::from_str::<awaken_provisioning_contract::SandboxHandle>(binding)
            .map_err(|_| crate::SessionMemoryReconciliationError::EnvironmentMismatch)?;
        let materializations = handle
            .memory_materializations()
            .map_err(|_| crate::SessionMemoryReconciliationError::EnvironmentMismatch)?
            .ok_or(crate::SessionMemoryReconciliationError::EnvironmentMismatch)?;
        if materializations
            .iter()
            .filter(|evidence| *evidence == intent.materialization())
            .count()
            != 1
        {
            return Err(crate::SessionMemoryReconciliationError::EnvironmentMismatch);
        }
        Ok(())
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

    fn environment_effect_context(
        &self,
        owner_scope: &str,
    ) -> Result<(String, Option<String>), crate::SessionEnvironmentReceiptError> {
        let environment_fingerprint = self
            .frozen_baseline()
            .map(|baseline| baseline.environment.config_fingerprint.0.clone())
            .ok_or(crate::SessionEnvironmentReceiptError::EnvironmentMismatch)?;
        let transition_fingerprint = if self.resources.pending.is_some() {
            let (previous_revision, previous) = self.resources.active_generation();
            let (desired_revision, desired) = self.resources.desired_generation();
            Some(
                crate::SessionResourceTransition::new(
                    crate::SessionResourceManifest::at_revision(
                        owner_scope,
                        previous_revision,
                        previous.clone(),
                    ),
                    crate::SessionResourceManifest::at_revision(
                        owner_scope,
                        desired_revision,
                        desired.clone(),
                    ),
                )
                .map_err(|_| crate::SessionEnvironmentReceiptError::ResourceTransitionMismatch)?
                .operation_fingerprint(),
            )
        } else {
            None
        };
        Ok((environment_fingerprint, transition_fingerprint))
    }

    /// Authorize one Environment effect against the complete Session aggregate
    /// before a provider is allowed to perform I/O. The same state-machine
    /// result is consumed by receipt application after the physical effect.
    pub fn authorize_environment_effect(
        &self,
        owner_scope: &str,
        intent: &crate::SessionEnvironmentEffectIntent,
        now_unix_ms: u64,
    ) -> Result<crate::SessionEnvironmentEffectAuthorization, crate::SessionEnvironmentReceiptError>
    {
        if intent.session_id() != self.session_id {
            return Err(crate::SessionEnvironmentReceiptError::Mismatch);
        }
        let realization_is_current = match (self.realization.as_ref(), intent.realization()) {
            (Some(current), Some(asserted)) => {
                crate::realization_lease_authorizes(current, asserted, now_unix_ms)
            }
            (None, None) => true,
            _ => false,
        };
        if !realization_is_current {
            return Err(crate::SessionEnvironmentReceiptError::RealizationStale);
        }
        let (environment_fingerprint, transition_fingerprint) =
            self.environment_effect_context(owner_scope)?;
        self.environment.authorize_effect(
            intent,
            &environment_fingerprint,
            transition_fingerprint.as_deref(),
        )
    }

    /// Apply an Environment receipt against the complete Session aggregate.
    /// The Environment state machine owns phase/source/substrate admission;
    /// this root method supplies its immutable baseline and the sole durable
    /// active→pending Resource operation identity.
    pub fn try_apply_environment_receipt(
        &mut self,
        owner_scope: &str,
        receipt: &crate::SessionEnvironmentReceipt,
        now_unix_ms: u64,
    ) -> Result<bool, crate::SessionEnvironmentReceiptError> {
        receipt.verify()?;
        let intent = receipt.effect_intent()?;
        let authorization = self.authorize_environment_effect(owner_scope, &intent, now_unix_ms)?;
        self.environment
            .apply_authorized_receipt(receipt, authorization)
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
