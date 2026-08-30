//! Persistence for the adapter-side Managed session aggregate.
//!
//! The runtime's committed transcript (ADR-0039) is durable, but the wire
//! `Session` object carries configuration that is NOT in the transcript — the
//! bound agent, the resolved model, the title/metadata, and the accepted MCP
//! servers. Without persisting it, a session rehydrated after a restart (or first
//! seen by another process sharing the store) reports placeholder defaults
//! (`agent = "assistant"`, empty `mcp_servers`, no title). This port stores that
//! aggregate so rehydration restores the real values.
//!
//! Secrets never cross this port. MCP and Repository entries may persist an
//! exact secret-free credential access/holder pin, but never credential material;
//! realization consumes that pin through the common exact resolver without
//! selecting another source or revision.

use async_trait::async_trait;
use std::collections::{BTreeMap, BTreeSet};

mod environment_binding;
mod repository_publication;
mod runtime_intervals;

pub use repository_publication::SessionArchiveWithRepositoryPublicationError;

use crate::ManagedLifecycleFact;

mod execution_state;
pub use execution_state::{
    SessionExecutionState, SessionExecutionStateError, SessionExecutionTransitionError,
};

/// Secret-free active MCP projection consumed by protocol adapters. It is
/// derived from the typed attachment aggregate without a JSON serialization hop.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct VisibleMcpServer {
    pub name: String,
    pub target: crate::McpTarget,
    pub prompts_as_skills: bool,
}

/// Monotonic root revision for every mutation of one Session aggregate.
#[derive(
    Clone,
    Copy,
    Debug,
    Default,
    PartialEq,
    Eq,
    PartialOrd,
    Ord,
    serde::Serialize,
    serde::Deserialize,
)]
#[serde(transparent)]
pub struct SessionRevision(pub u64);

/// Durable retention and public-visibility state of one Session.
///
/// `Deleting` is committed before physical cleanup starts. `Deleted` is retained
/// for imported historical rows and compact tombstone projections; ordinary new
/// deletions remove the aggregate only after cleanup receipts are committed.
#[derive(Clone, Debug, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(tag = "state", rename_all = "snake_case")]
pub enum SessionDisposition {
    #[default]
    Active,
    Archived {
        archived_at: String,
    },
    Deleting,
    Deleted,
}

impl SessionDisposition {
    #[must_use]
    pub const fn denies_activity(&self) -> bool {
        !matches!(self, Self::Active)
    }

    #[must_use]
    pub const fn is_hidden(&self) -> bool {
        matches!(self, Self::Deleting | Self::Deleted)
    }

    #[must_use]
    pub fn archived_at(&self) -> Option<&str> {
        match self {
            Self::Archived { archived_at } => Some(archived_at),
            Self::Active | Self::Deleting | Self::Deleted => None,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(u8)]
enum SessionDeleteDispositionClass {
    Active,
    Archived,
    Deleting,
    Deleted,
}

impl From<&SessionDisposition> for SessionDeleteDispositionClass {
    fn from(value: &SessionDisposition) -> Self {
        match value {
            SessionDisposition::Active => Self::Active,
            SessionDisposition::Archived { .. } => Self::Archived,
            SessionDisposition::Deleting => Self::Deleting,
            SessionDisposition::Deleted => Self::Deleted,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct SessionDeleteRequestPlan {
    transition_to_deleting: bool,
    terminalize_execution: bool,
    request_cleanup: bool,
}

/// Closed reducer plan for the durable Delete-intent transaction. Deleting and
/// Deleted are absorbing replays; every admitted request hides the aggregate,
/// fences nonterminal execution, and requests recoverable cleanup together.
#[must_use]
const fn session_delete_request_plan(
    disposition: SessionDeleteDispositionClass,
    execution_terminal: bool,
) -> SessionDeleteRequestPlan {
    let transition_to_deleting = matches!(
        disposition,
        SessionDeleteDispositionClass::Active | SessionDeleteDispositionClass::Archived
    );
    SessionDeleteRequestPlan {
        transition_to_deleting,
        terminalize_execution: transition_to_deleting && !execution_terminal,
        request_cleanup: transition_to_deleting,
    }
}

#[derive(Clone, Copy, Debug, thiserror::Error, PartialEq, Eq)]
pub enum SessionDispositionTransitionError {
    #[error("cannot archive a Session while deletion is in progress or complete")]
    ArchiveAfterDelete,
}

/// Durable owner fence for all process-local Session projections. Runtime and
/// Worker identities are opaque to the Session domain.
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct SessionRealizationLease {
    pub owner: String,
    pub runtime_incarnation: String,
    pub epoch: u64,
    pub expires_at_unix_ms: u64,
}

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

/// Closed admission kernel for physically deleting the authoritative Session
/// row. Visibility, execution fencing, accepted Event provenance, and verified
/// cleanup are independent axes; omitting any one of them fails closed.
#[must_use]
pub const fn session_tombstone_is_admitted(
    disposition_hidden: bool,
    execution_terminal: bool,
    cleanup_completed: bool,
    event_batches_complete: bool,
    session_identity_exact: bool,
    next_revision_exact: bool,
) -> bool {
    disposition_hidden
        && execution_terminal
        && cleanup_completed
        && event_batches_complete
        && session_identity_exact
        && next_revision_exact
}

/// One durable Session together with its intrinsic Workspace partition.
///
/// Recovery consumes this envelope atomically instead of looking up an owner in
/// a second step. It contains no principal, role, policy, credential, or
/// authorization decision; `workspace_id` is resource routing state only.
#[derive(Clone, Debug, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct ScopedPersistedSession {
    pub workspace_id: String,
    pub session: PersistedSession,
}

#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct SessionTombstone {
    pub session_id: String,
    pub deleted_revision: SessionRevision,
    pub deleted_at: String,
}

#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct IdempotencyRecord {
    pub key: String,
    pub payload_hash: String,
}

#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct SessionIdempotencyReceipt {
    pub payload_hash: String,
    pub committed_revision: SessionRevision,
}

/// Atomic outcome of one Session-root create command. Both variants carry the
/// repository's durable aggregate; callers must never continue from their
/// locally compiled candidate after the repository classified a replay.
#[derive(Clone, Debug, PartialEq)]
pub enum SessionCreateResult {
    Applied(PersistedSession),
    Replayed(PersistedSession),
}

#[derive(Clone, Debug, PartialEq, serde::Serialize, serde::Deserialize)]
#[allow(
    clippy::large_enum_variant,
    reason = "the public root-mutation contract intentionally carries a complete replacement aggregate"
)]
pub enum SessionMutationPayload {
    Replace(PersistedSession),
    Delete(SessionTombstone),
}

impl SessionMutationPayload {
    #[must_use]
    pub fn session_id(&self) -> &str {
        match self {
            Self::Replace(session) => &session.session_id,
            Self::Delete(tombstone) => &tombstone.session_id,
        }
    }

    #[must_use]
    pub fn stable_hash(&self) -> String {
        crate::stable_fingerprint(self)
    }
}

#[derive(Clone, Debug, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct SessionMutation {
    pub expected_revision: SessionRevision,
    pub idempotency: IdempotencyRecord,
    pub payload: SessionMutationPayload,
    #[serde(default)]
    pub lifecycle_facts: Vec<ManagedLifecycleFact>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum SessionMutationResult {
    Applied { new_revision: SessionRevision },
    Replayed { new_revision: SessionRevision },
    Conflict { current_revision: SessionRevision },
    IdempotencyMismatch,
}

#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum SessionMutationValidationError {
    #[error("Session mutation idempotency key is empty")]
    EmptyIdempotencyKey,
    #[error("Session mutation payload hash is empty")]
    EmptyPayloadHash,
    #[error("Session mutation id is empty")]
    EmptySessionId,
    #[error("Session mutation revision is exhausted")]
    RevisionExhausted,
    #[error("replacement carries a revision different from expected_revision")]
    ReplacementRevisionMismatch,
    #[error("tombstone deleted_revision is not the next root revision")]
    TombstoneRevisionMismatch,
    #[error("lifecycle fact targets another Session")]
    LifecycleSessionMismatch,
    #[error("Session Running interval is inconsistent with execution state")]
    RuntimeIntervalStateMismatch,
    #[error("Session active activity epochs are inconsistent with execution state or fence")]
    ActiveActivityStateMismatch,
    #[error("lifecycle runtime interval payload is inconsistent")]
    RuntimeIntervalFactMismatch,
}

#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum SessionRepositoryConflict {
    #[error("Session already exists")]
    AlreadyExists,
    #[error("Session was deleted")]
    Tombstoned,
    #[error("Session idempotency key was reused with another payload")]
    IdempotencyMismatch,
}

#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum SessionRepositoryError {
    #[error("Session was not found")]
    NotFound,
    #[error("Session repository is unavailable: {0}")]
    Unavailable(String),
    #[error("Session repository contains corrupt durable state: {0}")]
    Corrupt(String),
    #[error("Session repository conflict: {0}")]
    Conflict(SessionRepositoryConflict),
    #[error("Session repository rejected invalid mutation: {0}")]
    InvalidMutation(String),
}

/// Closed supervisor policy for persistence failures. Retryable outages remain
/// pending; corrupt durable truth is isolated for operator repair; command and
/// concurrency failures are returned without background replay.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SessionRepositoryRecoveryAction {
    Retry,
    Quarantine,
    Reject,
}

/// One corrupt durable Session row isolated by the authoritative store scan.
/// The raw aggregate is deliberately absent so recovery reporting cannot leak
/// persisted configuration or credentials.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SessionRecoveryQuarantine {
    pub session_id: String,
    pub reason: String,
}

/// Complete result of one recovery scan. Store adapters own row decoding and
/// durable quarantine, so callers receive healthy work and isolation evidence
/// from one authority instead of maintaining a parallel recovery registry.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct SessionRecoveryScan {
    pub sessions: Vec<ScopedPersistedSession>,
    pub quarantined: Vec<SessionRecoveryQuarantine>,
}

impl SessionRepositoryError {
    #[must_use]
    pub const fn recovery_action(&self) -> SessionRepositoryRecoveryAction {
        match self {
            Self::Unavailable(_) => SessionRepositoryRecoveryAction::Retry,
            Self::Corrupt(_) => SessionRepositoryRecoveryAction::Quarantine,
            Self::NotFound | Self::Conflict(_) | Self::InvalidMutation(_) => {
                SessionRepositoryRecoveryAction::Reject
            }
        }
    }
}

impl SessionMutation {
    /// Validate all command-local causes before a repository reads or writes.
    /// A successful return is the only root revision the transaction may commit.
    pub fn validate(&self) -> Result<SessionRevision, SessionMutationValidationError> {
        if self.idempotency.key.trim().is_empty() {
            return Err(SessionMutationValidationError::EmptyIdempotencyKey);
        }
        if self.idempotency.payload_hash.trim().is_empty() {
            return Err(SessionMutationValidationError::EmptyPayloadHash);
        }
        let session_id = self.payload.session_id();
        if session_id.trim().is_empty() {
            return Err(SessionMutationValidationError::EmptySessionId);
        }
        let next = SessionRevision(
            self.expected_revision
                .0
                .checked_add(1)
                .ok_or(SessionMutationValidationError::RevisionExhausted)?,
        );
        match &self.payload {
            SessionMutationPayload::Replace(session)
                if session.revision != self.expected_revision =>
            {
                return Err(SessionMutationValidationError::ReplacementRevisionMismatch);
            }
            SessionMutationPayload::Delete(tombstone) if tombstone.deleted_revision != next => {
                return Err(SessionMutationValidationError::TombstoneRevisionMismatch);
            }
            SessionMutationPayload::Replace(_) | SessionMutationPayload::Delete(_) => {}
        }
        if let SessionMutationPayload::Replace(session) = &self.payload
            && session.running_interval.is_some()
            && session.execution != SessionExecutionState::Running
        {
            return Err(SessionMutationValidationError::RuntimeIntervalStateMismatch);
        }
        if let SessionMutationPayload::Replace(session) = &self.payload
            && (session
                .active_activity_epochs
                .iter()
                .any(|epoch| *epoch == 0 || *epoch > session.activity_epoch)
                || (!session.active_activity_epochs.is_empty()
                    && (session.execution == SessionExecutionState::Idle || session.is_terminal())))
        {
            return Err(SessionMutationValidationError::ActiveActivityStateMismatch);
        }
        if self
            .lifecycle_facts
            .iter()
            .any(|fact| fact.object_id != session_id)
        {
            return Err(SessionMutationValidationError::LifecycleSessionMismatch);
        }
        if self.lifecycle_facts.iter().any(|fact| {
            match (&fact.runtime_interval, fact.event_type.as_str()) {
                (None, "session.runtime_interval_closed") => true,
                (Some(interval), event_type) => {
                    event_type != "session.runtime_interval_closed"
                        || fact.id != interval.interval_id
                        || interval.ended_at_unix_ms < interval.started_at_unix_ms
                }
                (None, _) => false,
            }
        }) {
            return Err(SessionMutationValidationError::RuntimeIntervalFactMismatch);
        }
        if self
            .lifecycle_facts
            .iter()
            .any(|fact| fact.runtime_interval.is_some())
            && matches!(
                &self.payload,
                SessionMutationPayload::Replace(session)
                    if session.execution == SessionExecutionState::Running
                        || session.running_interval.is_some()
            )
        {
            return Err(SessionMutationValidationError::RuntimeIntervalFactMismatch);
        }
        Ok(next)
    }
}

/// The port the Managed adapter drives to persist and restore [`PersistedSession`]
/// rows. The default in-memory impl keeps single-process behavior; a durable impl
/// (e.g. SQLite alongside the transcript store) lets a session survive a restart
/// and be reported faithfully by another process.
#[async_trait]
pub trait ManagedSessionRepository: Send + Sync {
    /// Insert one new aggregate together with owner, idempotency and outbox, or
    /// atomically return the exact durable aggregate for an owner-bound replay.
    async fn create(
        &self,
        owner_scope: &str,
        session: PersistedSession,
        idempotency: IdempotencyRecord,
        lifecycle_facts: Vec<ManagedLifecycleFact>,
    ) -> Result<SessionCreateResult, SessionRepositoryError>;

    /// Atomically classify a deterministic create identity without inserting.
    /// `Ok(None)` means both receipt and identity are absent. An exact receipt
    /// returns the current durable aggregate; occupied, tombstoned, mismatched,
    /// or corrupt identities retain the same typed result as [`Self::create`].
    async fn replay_create(
        &self,
        owner_scope: &str,
        session_id: &str,
        idempotency: &IdempotencyRecord,
    ) -> Result<Option<PersistedSession>, SessionRepositoryError>;

    /// Commit the one root-revision CAS transaction.
    async fn commit_mutation(
        &self,
        owner_scope: &str,
        mutation: SessionMutation,
    ) -> Result<SessionMutationResult, SessionRepositoryError>;

    /// Commit a lifecycle transition fact idempotently by stable id.
    async fn append_lifecycle(
        &self,
        fact: ManagedLifecycleFact,
    ) -> Result<(), SessionRepositoryError>;

    async fn pending_lifecycle(&self) -> Result<Vec<ManagedLifecycleFact>, SessionRepositoryError>;

    async fn complete_lifecycle(&self, fact_id: &str) -> Result<(), SessionRepositoryError>;

    /// The stored configuration for `session_id`. Absence is the typed
    /// [`SessionRepositoryError::NotFound`] case, never a storage fallback.
    async fn get(&self, session_id: &str) -> Result<PersistedSession, SessionRepositoryError>;

    /// Every visible aggregate owned by one Workspace. Collection reads must
    /// come from durable truth so a process restart cannot make existing
    /// Sessions disappear merely because the protocol projection cache is cold.
    async fn list_by_owner(
        &self,
        _owner_scope: &str,
    ) -> Result<Vec<PersistedSession>, SessionRepositoryError> {
        Err(SessionRepositoryError::Unavailable(
            "Session repository does not support Workspace listing".into(),
        ))
    }

    /// Sessions carrying any durable Resource, MCP, environment, or WorkQueue
    /// projection reconciliation work.
    /// Implementations preserve the intrinsic Workspace partition in the same
    /// row scan; application coordinators filter by their owned state machine.
    /// One index avoids parallel per-feature recovery registries and scans.
    async fn reconcilable_sessions(&self) -> Result<SessionRecoveryScan, SessionRepositoryError>;

    /// Count one typed Environment phase across every canonical live Session
    /// row. Unsupported adapters fail closed; a recovery batch is not global.
    async fn count_environment_phase(
        &self,
        _phase: crate::SessionEnvironmentPhase,
    ) -> Result<u64, SessionRepositoryError> {
        Err(SessionRepositoryError::Unavailable(
            "Session repository does not support global Environment phase counts".into(),
        ))
    }

    /// Healthy Sessions whose immutable authoring baseline references a Vault.
    /// This is the durable index used by credential rollout controllers; an
    /// in-memory cache or one process's active-runtime list is never complete in
    /// HA. Adapters that cannot provide the scan fail closed and leave the
    /// credential outbox event pending.
    async fn sessions_referencing_vault(
        &self,
        _workspace_id: &str,
        _vault_id: &str,
    ) -> Result<Vec<PersistedSession>, SessionRepositoryError> {
        Err(SessionRepositoryError::Unavailable(
            "Session repository does not support Vault rollout indexing".into(),
        ))
    }

    /// Healthy Sessions whose current desired MCP attachments reference one
    /// exact credential source. The Workspace comes from the Session root row;
    /// callers supply a source id only from the credential authority's exact
    /// committed rollout provenance. This index is additive to immutable Vault
    /// authoring references: neither can be inferred from or substituted for the
    /// other.
    async fn sessions_referencing_credential_source(
        &self,
        workspace_id: &str,
        source_id: &awaken_credential_contract::CredentialSourceId,
    ) -> Result<Vec<PersistedSession>, SessionRepositoryError>;

    /// Durable application-command receipt. This is a read of the same
    /// idempotency table written atomically by `create`/`commit_mutation`, not a
    /// second command registry.
    async fn idempotency_receipt(
        &self,
        session_id: &str,
        key: &str,
    ) -> Result<Option<SessionIdempotencyReceipt>, SessionRepositoryError>;

    /// The atomically persisted owner scope of `session_id`.
    async fn owner(&self, session_id: &str) -> Result<String, SessionRepositoryError>;
}

// In-memory and durable adapters live outward in `awaken-session-store`.
// Workspace ownership is persisted atomically beside each row through `create`;
// authorization scope decorators do not belong in this resource persistence port.

#[cfg(test)]
mod mutation_tests {
    use super::*;

    #[derive(Clone, Copy)]
    enum PayloadKind {
        Replace,
        Delete,
    }

    #[derive(Clone)]
    struct Rule {
        id: &'static str,
        payload: PayloadKind,
        key_nonempty: bool,
        hash_nonempty: bool,
        session_id_nonempty: bool,
        revision_available: bool,
        payload_revision_exact: bool,
        lifecycle_session_exact: bool,
        expected: Result<SessionRevision, SessionMutationValidationError>,
    }

    pub(super) fn session(id: &str, revision: SessionRevision) -> PersistedSession {
        use awaken_credential_contract::{
            CredentialRealizationProfile, PlaintextBoundary, PlaintextHolder,
        };

        PersistedSession {
            session_id: id.into(),
            revision,
            baseline: crate::SessionBaselineState::Preparing(crate::SessionCreationIntent {
                control: crate::ControlSessionCreationInputs {
                    mutation_policy: crate::SessionMutationPolicy::Managed,
                    environment: crate::EnvironmentSnapshot {
                        environment_id: "environment".into(),
                        revision: awaken_environment_contract::EnvironmentRevision(1),
                        self_hosted: false,
                        config_fingerprint: crate::EnvironmentFingerprint("config".into()),
                        sandbox: Default::default(),
                        sandbox_provisioning: Default::default(),
                        idle_retention: Default::default(),
                        packages: Default::default(),
                        prepared_image: None,
                        network: crate::SessionNetworkPolicy::Unrestricted,
                        credential_realization: CredentialRealizationProfile {
                            inference_holder: PlaintextHolder::new(
                                PlaintextBoundary::Workload,
                                "awaken.workload.acp",
                            ),
                            mcp_holder: PlaintextHolder::new(
                                PlaintextBoundary::Worker,
                                "awaken.worker",
                            ),
                            resource_holder: PlaintextHolder::new(
                                PlaintextBoundary::Worker,
                                "awaken.worker",
                            ),
                        },
                    },
                    runtime_placement: crate::SessionRuntimePlacement::Local,
                    agent_id: "assistant".into(),
                    agent_revision: None,
                    model: "model".into(),
                    execution_model_ref: "model".into(),
                    model_override: None,
                    system_prompt: crate::SessionSystemPromptSelection::Inherit,
                    runtime: None,
                    mcp_authoring: Default::default(),
                    toolsets: Vec::new(),
                    delegate_ids: Vec::new(),
                    mounts: Vec::new(),
                    env: Vec::new(),
                    prompts: Vec::new(),
                    transcript_prefix: None,
                    resources: Default::default(),
                    initial_mcp: Vec::new(),
                },
            }),
            title: None,
            metadata: Default::default(),
            tools: Default::default(),
            event_batches: Vec::new(),
            activity_epoch: 0,
            active_activity_epochs: Default::default(),
            running_interval: None,
            closed_runtime_intervals: Vec::new(),
            runtime_active_millis: 0,
            usage_cursor: Default::default(),
            budget: Default::default(),
            environment: Default::default(),
            mcp: Default::default(),
            resources: Default::default(),
            realization: None,
            realization_progress: Default::default(),
            execution: SessionExecutionState::Idle,
            disposition: SessionDisposition::Active,
            terminal_cleanup: Default::default(),
        }
    }

    #[test]
    fn frozen_constructor_owns_the_complete_initial_aggregate_shape() {
        // Cause/effect graph: C1 complete typed creation input is compiled; C2
        // no durable mutation has occurred. Effects: E1 every caller gets one
        // frozen baseline with Preparing execution, zero revision/activity,
        // complete effect aggregates, and exact presentation data; E2 no
        // protocol can construct a durable partially-authored root.
        //
        // | Rule | typed input | prior mutation | Effect |
        // | C1 | complete/frozen | no | E1 canonical aggregate |
        // | C2 | wire-specific defaults | no | E2 impossible at constructor |
        // Constraints/invariants: construction starts at revision/activity zero
        // with one complete frozen baseline; adapters cannot author partial roots.
        let fixture = session("constructor-source", SessionRevision(0));
        let crate::SessionBaselineState::Preparing(intent) = fixture.baseline else {
            panic!("fixture carries a creation intent");
        };
        let compiled = intent.finalize().expect("complete fixture compiles");
        let metadata = BTreeMap::from([("key".to_string(), "value".to_string())]);
        let prepared = PersistedSession::frozen_with_budget(
            "constructor",
            compiled.baseline,
            Default::default(),
            Default::default(),
            Some("title".into()),
            metadata.clone(),
            Default::default(),
            Default::default(),
        );
        assert_eq!(prepared.session_id, "constructor", "C1/E1");
        assert_eq!(prepared.revision, SessionRevision(0), "C1/E1");
        assert_eq!(
            prepared.execution,
            SessionExecutionState::Preparing,
            "C1/E1"
        );
        assert_eq!(prepared.disposition, SessionDisposition::Active, "C1/E1");
        assert_eq!(prepared.activity_epoch, 0, "C1/E1");
        assert!(prepared.active_activity_epochs.is_empty(), "C1/E1");
        assert!(prepared.frozen_baseline().is_some(), "C1/E1");
        assert_eq!(prepared.title.as_deref(), Some("title"), "C1/E1");
        assert_eq!(prepared.metadata, metadata, "C1/E1");
        assert!(prepared.mcp.attachments.is_empty(), "C1/E1");
        assert!(prepared.resources.active.inputs().is_empty(), "C1/E1");
        assert!(prepared.realization.is_none(), "C1/E1");
        assert!(
            matches!(
                prepared.terminal_cleanup,
                crate::SessionCleanupOperation::NotRequested
            ),
            "C1/E1"
        );
    }

    #[test]
    fn persisted_session_accepts_only_the_complete_canonical_grammar() {
        // Grammar cause/effect partition: C1 complete canonical aggregate;
        // C2 unknown status; C3 missing pre-existing required fact; C4 missing
        // Event-batch truth; C5 missing active-activity truth; C6 old status
        // alias; C7 unknown top-level fact. E1 is exact decode and E2 is a
        // fail-closed decode error. Rules S1=C1=>E1 and S2..S7=C2..C7=>E2.
        // Recovery never synthesizes domain truth from defaults, SQL columns,
        // or alternate spellings.
        // Constraints/invariants: the persisted aggregate has one closed grammar;
        // missing, aliased, or unknown facts never receive compatibility defaults.
        let value = serde_json::to_value(session("session-1", SessionRevision(1))).unwrap();
        assert_eq!(value.get("status"), Some(&serde_json::json!("idle")));
        assert!(value.get("lifecycle").is_none());
        assert!(
            serde_json::from_value::<PersistedSession>(value.clone()).is_ok(),
            "S1"
        );

        let mut unknown = value.clone();
        unknown["status"] = serde_json::json!("legacy-unknown");
        assert!(
            serde_json::from_value::<PersistedSession>(unknown).is_err(),
            "S2"
        );

        let mut missing = value.clone();
        missing
            .as_object_mut()
            .unwrap()
            .remove("realization_progress");
        assert!(
            serde_json::from_value::<PersistedSession>(missing).is_err(),
            "S3"
        );

        let mut missing_batches = value.clone();
        missing_batches
            .as_object_mut()
            .unwrap()
            .remove("event_batches");
        assert!(
            serde_json::from_value::<PersistedSession>(missing_batches).is_err(),
            "S4"
        );

        let mut missing_activities = value.clone();
        missing_activities
            .as_object_mut()
            .unwrap()
            .remove("active_activity_epochs");
        assert!(
            serde_json::from_value::<PersistedSession>(missing_activities).is_err(),
            "S5"
        );

        let mut aliased = value.clone();
        let status = aliased.as_object_mut().unwrap().remove("status").unwrap();
        aliased["lifecycle"] = status;
        assert!(
            serde_json::from_value::<PersistedSession>(aliased).is_err(),
            "S6"
        );

        let mut extra = value;
        extra["unknown"] = serde_json::json!(true);
        assert!(
            serde_json::from_value::<PersistedSession>(extra).is_err(),
            "S7"
        );
    }

    #[test]
    fn repository_failure_policy_is_closed_and_exhaustive() {
        use SessionRepositoryError as Error;
        use SessionRepositoryRecoveryAction as Action;

        assert_eq!(
            Error::Unavailable("db offline".into()).recovery_action(),
            Action::Retry
        );
        assert_eq!(
            Error::Corrupt("negative revision".into()).recovery_action(),
            Action::Quarantine
        );
        assert_eq!(Error::NotFound.recovery_action(), Action::Reject);
        assert_eq!(
            Error::Conflict(SessionRepositoryConflict::AlreadyExists).recovery_action(),
            Action::Reject
        );
        assert_eq!(
            Error::InvalidMutation("bad revision".into()).recovery_action(),
            Action::Reject
        );
    }

    #[test]
    fn execution_transition_decision_table_fails_closed() {
        // Test design — Causes: every execution-state pair and the Idle/Running
        // activity-admission partition are evaluated. Effects: permitted pairs
        // mutate atomically and terminal entry clears activities; forbidden pairs
        // preserve the aggregate. Constraints/invariants: Terminated is absorbing,
        // activity begins only from Idle/Running, and no rejected edge mutates.
        // Decision rule X1: the explicit closed transition relation below is true
        // iff `can_transition_to` and `transition_execution` admit the same edge.
        use SessionExecutionState as State;

        let states = [
            State::Preparing,
            State::Activating,
            State::ActivationFailed,
            State::Running,
            State::Rescheduling,
            State::Idle,
            State::Terminated,
        ];
        for from in states {
            // Activity-admission decision table: Idle opens the first interval;
            // Running joins/replaces a live or crash-orphaned epoch; every
            // realization/reschedule/terminal state rejects before Runtime.
            // FMECA: classifying only Idle as ready strands a crash-orphaned
            // Running activity, while admitting any other state bypasses
            // realization or terminal fencing.
            assert_eq!(
                from.admits_activity(),
                matches!(from, State::Idle | State::Running),
                "activity admission from {from}"
            );
            for to in states {
                let expected = from == to
                    || (!from.is_terminal()
                        && match to {
                            State::Terminated => true,
                            State::ActivationFailed => from != State::Idle,
                            State::Activating => matches!(
                                from,
                                State::Preparing | State::Running | State::Rescheduling
                            ),
                            State::Idle => matches!(
                                from,
                                State::Preparing
                                    | State::Activating
                                    | State::Running
                                    | State::Rescheduling
                            ),
                            State::Running => from == State::Idle,
                            State::Rescheduling => matches!(from, State::Idle | State::Running),
                            State::Preparing => false,
                        });
                assert_eq!(from.can_transition_to(to), expected, "{from} -> {to}");
            }
        }

        let mut value = session("session-1", SessionRevision(1));
        assert_eq!(value.transition_execution(State::Idle), Ok(false));
        assert_eq!(value.transition_execution(State::Running), Ok(true));
        assert_eq!(value.execution, State::Running);
        assert_eq!(value.begin_activity_epoch(), Some(1));
        assert_eq!(value.active_activity_epochs, BTreeSet::from([1]));
        assert_eq!(value.transition_execution(State::Terminated), Ok(true));
        assert!(
            value.active_activity_epochs.is_empty(),
            "terminal transition clears every active activity"
        );
        let terminal = value.clone();
        assert_eq!(
            value.transition_execution(State::Idle),
            Err(SessionExecutionTransitionError {
                from: State::Terminated,
                to: State::Idle,
            })
        );
        assert_eq!(value, terminal, "rejected transition must be atomic");
    }

    #[test]
    fn disposition_is_orthogonal_to_execution_and_delete_is_idempotent() {
        let mut archived = session("archived", SessionRevision(1));
        assert_eq!(archived.archive("2026-08-08T00:00:00Z"), Ok(true));
        assert_eq!(archived.execution, SessionExecutionState::Terminated);
        assert_eq!(archived.archived_at(), Some("2026-08-08T00:00:00Z"));
        assert!(archived.request_delete());
        assert!(archived.is_hidden());
        assert!(!archived.request_delete(), "delete replay is idempotent");

        let mut failed = session("failed", SessionRevision(1));
        failed.execution = SessionExecutionState::ActivationFailed;
        assert!(failed.is_terminal());
        assert!(
            failed.is_publicly_readable(),
            "failed async creation remains exactly queryable"
        );
        assert!(failed.request_delete(), "failed Sessions remain deletable");
        assert_eq!(failed.execution, SessionExecutionState::ActivationFailed);
        assert!(matches!(failed.disposition, SessionDisposition::Deleting));
    }

    /// Cause graph: lifecycle fact -> terminal classification -> realization and
    /// Resource-cleanup eligibility. `pending` means the Resource aggregate owns
    /// unfinished work; `active` means it has a resident manifest.
    ///
    /// | Rule | status | pending | active | Terminal | Resource reconcile |
    /// |---|---|---|---|---|---|
    /// | L1 | preparing | false | true | false | false |
    /// | L2 | running | false | true | false | false |
    /// | L3 | rescheduling | false | true | false | false |
    /// | L4 | idle | false | true | false | false |
    /// | L5 | idle | true | any | false | true |
    /// | L6 | terminated | false | true | true | true |
    /// | L7 | terminated | false | false | true | false |
    /// | L8 | deleted | false | false | true | true |
    /// | L9 | activation_failed | false | true | true | true |
    #[test]
    fn terminal_state_classification_follows_the_decision_table() {
        for (rule, status, disposition, pending, active, terminal, resource_reconcile) in [
            (
                "L1",
                "preparing",
                SessionDisposition::Active,
                false,
                true,
                false,
                false,
            ),
            (
                "L2",
                "running",
                SessionDisposition::Active,
                false,
                true,
                false,
                false,
            ),
            (
                "L3",
                "rescheduling",
                SessionDisposition::Active,
                false,
                true,
                false,
                false,
            ),
            (
                "L4",
                "idle",
                SessionDisposition::Active,
                false,
                true,
                false,
                false,
            ),
            (
                "L5",
                "idle",
                SessionDisposition::Active,
                true,
                false,
                false,
                true,
            ),
            (
                "L6",
                "terminated",
                SessionDisposition::Archived {
                    archived_at: "at".into(),
                },
                false,
                true,
                true,
                true,
            ),
            (
                "L7",
                "terminated",
                SessionDisposition::Archived {
                    archived_at: "at".into(),
                },
                false,
                false,
                true,
                false,
            ),
            (
                "L8",
                "terminated",
                SessionDisposition::Deleting,
                false,
                false,
                true,
                true,
            ),
            (
                "L9",
                "activation_failed",
                SessionDisposition::Active,
                false,
                true,
                true,
                true,
            ),
        ] {
            let mut value = session("session-1", SessionRevision(1));
            value.execution = status.parse().expect("fixture execution state");
            value.disposition = disposition;
            let desired = crate::ResolvedSessionResources::try_new(
                vec![crate::ResolvedInput {
                    binding_id: awaken_resource_contract::BindingId::from("input-1"),
                    mount_path: "input.txt".into(),
                    access: awaken_resource_contract::ResourceAccess::ReadOnly,
                    source: crate::ResolvedInputSource::File {
                        file_id: awaken_resource_contract::FileId::from("file-1"),
                    },
                    instructions: None,
                }],
                Vec::new(),
            )
            .unwrap();
            if active {
                value
                    .resources
                    .prepare("session-1", desired.clone())
                    .expect("active generation prepare");
                value.resources.start_attempt().expect("active attempt");
                value.resources.commit().expect("active commit");
            }
            if pending {
                value
                    .resources
                    .prepare("session-1", desired)
                    .expect("L5 pending generation");
            }
            assert_eq!(value.is_terminal(), terminal, "{rule}");
            assert_eq!(
                value.needs_resource_reconciliation(),
                resource_reconcile,
                "{rule}"
            );
        }
    }

    /// Cause-effect graph:
    ///
    /// C1 key present -> C2 hash present -> C3 Session id present
    /// -> C4 next revision exists -> C5 payload revision is exact
    /// -> C6 every lifecycle fact targets the same Session -> E1 next revision.
    /// Every failed cause yields its stable E2 validation error and no write.
    ///
    /// Decision table (`-` means evaluation already terminated):
    ///
    /// | Rule | Kind | C1 | C2 | C3 | C4 | C5 | C6 | Result |
    /// |---|---|---|---|---|---|---|---|---|
    /// | R1 | replace | T | T | T | T | T | T | revision 8 |
    /// | R2 | delete | T | T | T | T | T | T | revision 8 |
    /// | R3 | either | F | - | - | - | - | - | empty key |
    /// | R4 | either | T | F | - | - | - | - | empty hash |
    /// | R5 | either | T | T | F | - | - | - | empty id |
    /// | R6 | either | T | T | T | F | - | - | exhausted |
    /// | R7 | replace | T | T | T | T | F | - | replace mismatch |
    /// | R8 | delete | T | T | T | T | F | - | tombstone mismatch |
    /// | R9 | either | T | T | T | T | T | F | lifecycle mismatch |
    ///
    /// Constraint/invariant: validation is ordered and side-effect free; the
    /// first failed prerequisite returns its stable error and never advances a
    /// Session revision or partially accepts lifecycle facts.
    #[test]
    fn mutation_validation_tests_are_generated_from_the_decision_table() {
        let rules = [
            Rule {
                id: "R1",
                payload: PayloadKind::Replace,
                key_nonempty: true,
                hash_nonempty: true,
                session_id_nonempty: true,
                revision_available: true,
                payload_revision_exact: true,
                lifecycle_session_exact: true,
                expected: Ok(SessionRevision(8)),
            },
            Rule {
                id: "R2",
                payload: PayloadKind::Delete,
                key_nonempty: true,
                hash_nonempty: true,
                session_id_nonempty: true,
                revision_available: true,
                payload_revision_exact: true,
                lifecycle_session_exact: true,
                expected: Ok(SessionRevision(8)),
            },
            Rule {
                id: "R3",
                payload: PayloadKind::Replace,
                key_nonempty: false,
                hash_nonempty: true,
                session_id_nonempty: true,
                revision_available: true,
                payload_revision_exact: true,
                lifecycle_session_exact: true,
                expected: Err(SessionMutationValidationError::EmptyIdempotencyKey),
            },
            Rule {
                id: "R4",
                payload: PayloadKind::Replace,
                key_nonempty: true,
                hash_nonempty: false,
                session_id_nonempty: true,
                revision_available: true,
                payload_revision_exact: true,
                lifecycle_session_exact: true,
                expected: Err(SessionMutationValidationError::EmptyPayloadHash),
            },
            Rule {
                id: "R5",
                payload: PayloadKind::Replace,
                key_nonempty: true,
                hash_nonempty: true,
                session_id_nonempty: false,
                revision_available: true,
                payload_revision_exact: true,
                lifecycle_session_exact: true,
                expected: Err(SessionMutationValidationError::EmptySessionId),
            },
            Rule {
                id: "R6",
                payload: PayloadKind::Replace,
                key_nonempty: true,
                hash_nonempty: true,
                session_id_nonempty: true,
                revision_available: false,
                payload_revision_exact: true,
                lifecycle_session_exact: true,
                expected: Err(SessionMutationValidationError::RevisionExhausted),
            },
            Rule {
                id: "R7",
                payload: PayloadKind::Replace,
                key_nonempty: true,
                hash_nonempty: true,
                session_id_nonempty: true,
                revision_available: true,
                payload_revision_exact: false,
                lifecycle_session_exact: true,
                expected: Err(SessionMutationValidationError::ReplacementRevisionMismatch),
            },
            Rule {
                id: "R8",
                payload: PayloadKind::Delete,
                key_nonempty: true,
                hash_nonempty: true,
                session_id_nonempty: true,
                revision_available: true,
                payload_revision_exact: false,
                lifecycle_session_exact: true,
                expected: Err(SessionMutationValidationError::TombstoneRevisionMismatch),
            },
            Rule {
                id: "R9",
                payload: PayloadKind::Replace,
                key_nonempty: true,
                hash_nonempty: true,
                session_id_nonempty: true,
                revision_available: true,
                payload_revision_exact: true,
                lifecycle_session_exact: false,
                expected: Err(SessionMutationValidationError::LifecycleSessionMismatch),
            },
        ];

        for rule in rules {
            let expected_revision = if rule.revision_available {
                SessionRevision(7)
            } else {
                SessionRevision(u64::MAX)
            };
            let session_id = if rule.session_id_nonempty {
                "session-1"
            } else {
                ""
            };
            let next = expected_revision.0.checked_add(1).unwrap_or_default();
            let payload = match rule.payload {
                PayloadKind::Replace => SessionMutationPayload::Replace(session(
                    session_id,
                    if rule.payload_revision_exact {
                        expected_revision
                    } else {
                        SessionRevision(expected_revision.0.saturating_sub(1))
                    },
                )),
                PayloadKind::Delete => SessionMutationPayload::Delete(SessionTombstone {
                    session_id: session_id.into(),
                    deleted_revision: if rule.payload_revision_exact {
                        SessionRevision(next)
                    } else {
                        expected_revision
                    },
                    deleted_at: "2026-07-25T00:00:00Z".into(),
                }),
            };
            let mutation = SessionMutation {
                expected_revision,
                idempotency: IdempotencyRecord {
                    key: if rule.key_nonempty { "request-1" } else { "" }.into(),
                    payload_hash: if rule.hash_nonempty {
                        "sha256:payload"
                    } else {
                        ""
                    }
                    .into(),
                },
                payload,
                lifecycle_facts: vec![ManagedLifecycleFact {
                    id: "fact-1".into(),
                    object_id: if rule.lifecycle_session_exact {
                        session_id
                    } else {
                        "another-session"
                    }
                    .into(),
                    workspace_id: Some("workspace".into()),
                    event_type: "session.updated".into(),
                    timestamp: 1,
                    runtime_interval: None,
                }],
            };
            assert_eq!(
                mutation.validate(),
                rule.expected,
                "decision rule {}",
                rule.id
            );
        }
    }

    #[test]
    fn runtime_interval_mutation_decision_table_is_fail_closed() {
        // Cause/effect graph: C1=open interval with Running/non-Running state;
        // C2=closed payload absent/present; C3=event kind and stable id exact;
        // C4=end precedes start. Effects: E1 admits the exact aggregate/fact;
        // E2 rejects malformed or contradictory durable truth. Decision rules:
        // R1 Running+C1 => E1; R2 non-Running+C1 => E2; R3 exact C2+C3+!C4
        // => E1; R4-R6 missing/wrong-id/reversed payload => E2; C5 active
        // epochs are positive, no newer than the monotonic fence, and absent
        // from Idle or terminal state. R7 valid C5 => E1; R8/R9 future/Idle
        // active epochs => E2. This is the root-store boundary, so no adapter
        // can persist a billable parallel truth.
        // Constraints/invariants: open-interval state, active epochs, and the
        // matching closure fact form one atomic aggregate and one billing truth.
        let expected_revision = SessionRevision(7);
        let valid_interval = crate::SessionRuntimeInterval {
            interval_id: "interval-1".into(),
            activity_epoch: 3,
            started_at_unix_ms: 100,
            ended_at_unix_ms: 200,
            opened_revision: SessionRevision(7),
            closed_revision: SessionRevision(8),
            observations: Vec::new(),
            usage: Default::default(),
            max_list_cost_minor: None,
        };
        let mutation = |session: PersistedSession, fact: ManagedLifecycleFact| SessionMutation {
            expected_revision,
            idempotency: IdempotencyRecord {
                key: format!("interval:{}", fact.id),
                payload_hash: "payload".into(),
            },
            payload: SessionMutationPayload::Replace(session),
            lifecycle_facts: vec![fact],
        };
        let fact = |interval: Option<crate::SessionRuntimeInterval>| ManagedLifecycleFact {
            id: "interval-1".into(),
            object_id: "session-1".into(),
            workspace_id: Some("workspace".into()),
            event_type: "session.runtime_interval_closed".into(),
            timestamp: 1,
            runtime_interval: interval,
        };

        let mut running = session("session-1", expected_revision);
        running.execution = SessionExecutionState::Running;
        running.activity_epoch = 3;
        running.active_activity_epochs.insert(3);
        assert!(running.begin_runtime_interval(100), "R1 setup");
        let ordinary_fact = ManagedLifecycleFact {
            id: "ordinary".into(),
            object_id: "session-1".into(),
            workspace_id: Some("workspace".into()),
            event_type: "session.updated".into(),
            timestamp: 1,
            runtime_interval: None,
        };
        assert_eq!(
            mutation(running.clone(), ordinary_fact.clone()).validate(),
            Ok(SessionRevision(8)),
            "R1"
        );

        let mut idle_with_interval = running.clone();
        idle_with_interval.execution = SessionExecutionState::Idle;
        assert_eq!(
            mutation(idle_with_interval, fact(Some(valid_interval.clone()))).validate(),
            Err(SessionMutationValidationError::RuntimeIntervalStateMismatch),
            "R2"
        );
        let mut closed = running;
        closed.running_interval = None;
        closed.execution = SessionExecutionState::Idle;
        closed.active_activity_epochs.clear();
        assert_eq!(
            mutation(closed.clone(), fact(Some(valid_interval.clone()))).validate(),
            Ok(SessionRevision(8)),
            "R3"
        );
        let mut future_active = closed.clone();
        future_active.execution = SessionExecutionState::Running;
        future_active.active_activity_epochs.insert(4);
        assert_eq!(
            mutation(future_active, ordinary_fact.clone()).validate(),
            Err(SessionMutationValidationError::ActiveActivityStateMismatch),
            "R8"
        );
        let mut idle_active = closed.clone();
        idle_active.active_activity_epochs.insert(3);
        assert_eq!(
            mutation(idle_active, ordinary_fact).validate(),
            Err(SessionMutationValidationError::ActiveActivityStateMismatch),
            "R9"
        );
        assert_eq!(
            mutation(closed.clone(), fact(None)).validate(),
            Err(SessionMutationValidationError::RuntimeIntervalFactMismatch),
            "R4"
        );
        let mut wrong_id = valid_interval.clone();
        wrong_id.interval_id = "other".into();
        assert_eq!(
            mutation(closed.clone(), fact(Some(wrong_id))).validate(),
            Err(SessionMutationValidationError::RuntimeIntervalFactMismatch),
            "R5"
        );
        let mut reversed = valid_interval;
        reversed.ended_at_unix_ms = 99;
        assert_eq!(
            mutation(closed, fact(Some(reversed))).validate(),
            Err(SessionMutationValidationError::RuntimeIntervalFactMismatch),
            "R6"
        );
    }

    #[test]
    fn effective_runtime_usage_counts_open_interval_once() {
        // Active-time cause/effect table. C1 interval is closed/open; C2 now is
        // before/equal/after start; C3 the open interval is subsequently closed
        // at the same instant. E1 closed total only; E2 clamp clock rollback;
        // E3 add open elapsed once; E4 closing preserves the same effective
        // total (no double count). Rules: T1 closed=>E1; T2 open+before=>E2;
        // T3 open+after=>E3; T4 T3 then close=>E4. Repeating a rule is exact and
        // budget reconciliation separately keeps its cumulative cursor monotonic.
        // Constraints/invariants: elapsed time is nonnegative, monotonic, and an
        // open interval is included at most once before or after closure.
        let mut value = session("active-time", SessionRevision(1));
        value.runtime_active_millis = 2_000;
        assert_eq!(
            value.effective_runtime_active_millis(50_000),
            2_000,
            "T1/E1"
        );
        value.execution = SessionExecutionState::Running;
        value.activity_epoch = 1;
        value.active_activity_epochs.insert(1);
        assert!(value.begin_runtime_interval(10_000), "T2 setup");
        assert_eq!(value.effective_runtime_active_millis(9_000), 2_000, "T2/E2");
        assert_eq!(
            value.effective_runtime_active_millis(13_500),
            5_500,
            "T3/E3"
        );
        value.close_runtime_interval(13_500).expect("T4 close");
        assert_eq!(
            value.effective_runtime_active_millis(99_000),
            5_500,
            "T4/E4"
        );
    }
}

#[cfg(kani)]
mod verification {
    use super::{
        SessionDeleteDispositionClass, SessionExecutionState, session_delete_request_plan,
        session_tombstone_is_admitted,
    };

    #[kani::proof]
    fn terminal_execution_never_reopens() {
        let terminal = if kani::any::<bool>() {
            SessionExecutionState::ActivationFailed
        } else {
            SessionExecutionState::Terminated
        };
        let next = match kani::any::<u8>() % 7 {
            0 => SessionExecutionState::Preparing,
            1 => SessionExecutionState::Activating,
            2 => SessionExecutionState::ActivationFailed,
            3 => SessionExecutionState::Running,
            4 => SessionExecutionState::Rescheduling,
            5 => SessionExecutionState::Idle,
            _ => SessionExecutionState::Terminated,
        };
        if terminal.can_transition_to(next) {
            assert_eq!(terminal, next);
        }
    }

    #[kani::proof]
    fn session_tombstone_requires_hidden_disposition_terminal_execution_and_verified_cleanup() {
        let disposition_hidden = kani::any::<bool>();
        let execution_terminal = kani::any::<bool>();
        let cleanup_completed = kani::any::<bool>();
        let event_batches_complete = kani::any::<bool>();
        let session_identity_exact = kani::any::<bool>();
        let next_revision_exact = kani::any::<bool>();
        assert_eq!(
            session_tombstone_is_admitted(
                disposition_hidden,
                execution_terminal,
                cleanup_completed,
                event_batches_complete,
                session_identity_exact,
                next_revision_exact,
            ),
            disposition_hidden
                && execution_terminal
                && cleanup_completed
                && event_batches_complete
                && session_identity_exact
                && next_revision_exact
        );
    }

    #[kani::proof]
    fn session_delete_request_plan_is_exact_hidden_terminal_and_idempotent() {
        let disposition_code = kani::any::<u8>();
        kani::assume(disposition_code < 4);
        let disposition = match disposition_code {
            0 => SessionDeleteDispositionClass::Active,
            1 => SessionDeleteDispositionClass::Archived,
            2 => SessionDeleteDispositionClass::Deleting,
            3 => SessionDeleteDispositionClass::Deleted,
            _ => unreachable!(),
        };
        let execution_terminal = kani::any::<bool>();
        let plan = session_delete_request_plan(disposition, execution_terminal);
        let first_request = disposition_code < 2;
        assert_eq!(plan.transition_to_deleting, first_request);
        assert_eq!(plan.request_cleanup, first_request);
        assert_eq!(
            plan.terminalize_execution,
            first_request && !execution_terminal
        );
        if disposition_code >= 2 {
            assert!(!plan.transition_to_deleting);
            assert!(!plan.request_cleanup);
            assert!(!plan.terminalize_execution);
        }
    }
}
