// @formal entrypoint session.delete_retry_never_reopens_duplicates_fact_or_changes_cleanup_identity
// @formal entrypoint session.internal_create_accepts_only_a_complete_durable_root_and_recovers_without_a_job_store
// @formal entrypoint session_environment.terminal_fence_admits_only_inflight_binding_receipts
// @formal entrypoint session_event_batch.initial_plan_and_wake_activity_are_atomic_with_root
// @formal entrypoint session_event_batch.processed_requires_commit_observation_activity_and_dispatch_settlement
// @formal entrypoint session_execution.activity_receipts_are_unique_durable_and_exactly_settled
// @formal entrypoint session_execution.initial_event_path_reaches_idle_only_after_full_settlement
// @formal entrypoint session_resources.terminal_session_never_reactivates
//! The durable Managed Session aggregate and its root-owned projections.

use super::*;
use awaken_session_configuration_contract::VisibleMcpServer;

use crate::{
    SessionEventBatchError, SessionEventDispatchSettlementPlan,
    SessionEventDispatchSettlementReceipt, SessionEventProjectionAnchor,
    session_event_execution_may_close, session_inputs_allow_idle,
};

/// Whether this aggregate still represents a frozen preparation with no
/// admitted realization effects. This is only a state predicate: the
/// Session application must recheck it inside the archive root CAS and
/// join verified cleanup before returning a successful profiled release.
#[must_use]
pub fn session_is_unattempted_profiled_preparation(session: &PersistedSession) -> bool {
    session_has_unattempted_profiled_environment(session)
        && !session.has_active_activities()
        && session.running_interval.is_none()
        && session.mcp.attachments.iter().all(|attachment| {
            attachment.state
                == awaken_session_configuration_contract::mcp::McpAttachmentState::Requested
        })
}

/// Physical preparation facts shared by archive admission and credential
/// adoption. A reserved Run is logical activity, not a realization effect.
fn session_has_unattempted_profiled_environment(session: &PersistedSession) -> bool {
    !session
        .environment_materialization
        .as_deref()
        .is_some_and(crate::SessionEnvironmentMaterialization::has_admitted)
        && session.frozen_baseline().is_some_and(|baseline| {
            baseline.mutation_policy
                == awaken_session_configuration_contract::resource::SessionMutationPolicy::Frozen
        })
        && session.created_at_unix_ms.is_some()
        && session.execution == SessionExecutionState::Preparing
        && matches!(
            session.environment,
            crate::SessionEnvironmentState::Unmaterialized
        )
        && session.realization.is_none()
        && session.realization_progress.attempts == 0
        && session.realization_progress.last_error.is_none()
        && session.resources.active.inputs().is_empty()
        && session.resources.active.skills().is_empty()
        && session.resources.repository_retirements.is_empty()
        && session.resources.activations.iter().all(|activation| {
            activation.state == crate::ActivationState::Prepared
                && activation.attempts == 0
                && activation.lease_expires_at_unix_ms.is_none()
        })
        && session.mcp.attachments.iter().all(|attachment| {
            matches!(
                attachment.state,
                awaken_session_configuration_contract::mcp::McpAttachmentState::Requested
                    | awaken_session_configuration_contract::mcp::McpAttachmentState::Removed
            ) && attachment.attempts == 0
                && attachment.realization.is_none()
                && !attachment.publication_acknowledged
        })
}

/// Credential-only adoption may advance an unattempted frozen preparation even
/// after Run reservation. Missing materialization history is unknown. Callers
/// still validate same-source/topology monotonicity and recheck under root CAS.
#[must_use]
pub fn session_can_adopt_mcp_credential_lifecycle(session: &PersistedSession) -> bool {
    !session.is_terminal()
        && (session.execution == SessionExecutionState::Idle
            || (session_has_unattempted_profiled_environment(session)
                && session
                    .environment_materialization()
                    .is_some_and(crate::SessionEnvironmentMaterialization::is_empty)))
}

/// The aggregate's one terminality law: execution and retention are distinct
/// causes. Archived/deleting/deleted roots deny new activity even while their
/// execution projection has not yet been terminalized.
#[must_use]
pub const fn session_is_terminal(
    execution: SessionExecutionState,
    disposition: &SessionDisposition,
) -> bool {
    execution.is_terminal() || disposition.denies_activity()
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SessionEventDispatchSettlementEffect {
    NotEvent,
    Applied { fully_settled: bool },
    Replayed { fully_settled: bool },
}

/// A terminal aggregate admits an already-started Environment receipt only
/// while its cleanup fence is still joining in-flight work. Once the target set
/// is frozen, a late receipt would escape that set and must fail closed.
#[must_use]
const fn terminal_environment_receipt_admitted(
    session_is_terminal: bool,
    terminal_cleanup_is_fenced: bool,
) -> bool {
    !session_is_terminal || terminal_cleanup_is_fenced
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum InitialEventInstallationPlan {
    Reject,
    Install {
        admitted_revision: SessionRevision,
        wake_activity_epoch: u64,
    },
}

/// Decide the complete create-only Event coordinates before mutating the root.
/// Initial input is valid only on the untouched aggregate produced by
/// `frozen_with_budget`; both coordinates become the same first root revision.
#[must_use]
const fn initial_event_installation_plan(
    root_revision: SessionRevision,
    activity_epoch: u64,
    event_batches_empty: bool,
    active_activities_empty: bool,
) -> InitialEventInstallationPlan {
    if root_revision.0 != 0
        || activity_epoch != 0
        || !event_batches_empty
        || !active_activities_empty
    {
        InitialEventInstallationPlan::Reject
    } else {
        InitialEventInstallationPlan::Install {
            admitted_revision: SessionRevision(1),
            wake_activity_epoch: 1,
        }
    }
}

#[cfg(kani)]
/// @obligation session_environment.terminal_fence_admits_only_inflight_binding_receipts
/// @proof-input-debt tautology terminal_environment_receipt_admitted
#[kani::proof]
fn terminal_environment_receipt_is_admitted_only_during_the_fenced_join() {
    // Causes: C1 the Session is active/terminal; C2 terminal cleanup is in its
    // Fenced join or any other phase. Effects: E1 active receipt admission
    // remains delegated to the Environment kernel; E2 a terminal in-flight
    // receipt may join only in Fenced; E3 every other terminal receipt fails.
    // Rules: R1=!C1=>E1; R2=C1+C2=>E2; R3=C1+!C2=>E3.
    let terminal = kani::any::<bool>();
    let fenced = kani::any::<bool>();
    assert_eq!(
        terminal_environment_receipt_admitted(terminal, fenced),
        !terminal || fenced
    );
}

#[cfg(kani)]
/// @obligation session_event_batch.initial_plan_and_wake_activity_are_atomic_with_root
/// @obligation session_execution.initial_event_path_reaches_idle_only_after_full_settlement
/// @proof-input-debt free_symbol initial_event_installation_plan active_activities_empty
/// @proof-input-debt free_symbol initial_event_installation_plan event_batches_empty
#[kani::proof]
fn initial_event_plan_is_installed_only_on_the_complete_fresh_root() {
    // Cause/effect table covers arbitrary revision/epoch plus both collection
    // emptiness facts. R1: rev=epoch=0 and both empty => one Install whose
    // admitted revision and wake epoch are exactly 1. Every complementary row
    // is R2 Reject. Effect: no accepted plan can split batch and activity
    // coordinates before the repository performs its one root insert.
    let revision = SessionRevision(kani::any());
    let epoch = kani::any();
    let batches_empty = kani::any();
    let activities_empty = kani::any();
    let plan = initial_event_installation_plan(revision, epoch, batches_empty, activities_empty);
    let eligible = revision.0 == 0 && epoch == 0 && batches_empty && activities_empty;
    assert_eq!(
        matches!(plan, InitialEventInstallationPlan::Install { .. }),
        eligible
    );
    if let InitialEventInstallationPlan::Install {
        admitted_revision,
        wake_activity_epoch,
    } = plan
    {
        assert_eq!(admitted_revision, SessionRevision(1));
        assert_eq!(wake_activity_epoch, admitted_revision.0);
    }
}

#[cfg(test)]
#[test]
fn initial_event_installation_decision_table_is_closed() {
    // Causes: root revision, activity epoch, retained batches and active
    // activities are fresh/stale. Effects: only the all-fresh row installs
    // batch revision 1 plus the same wake epoch; every one-axis stale row
    // rejects. This native table is the boundary companion to the exhaustive
    // Kani proof and the application create/replay integration cases.
    let fresh = initial_event_installation_plan(SessionRevision(0), 0, true, true);
    assert_eq!(
        fresh,
        InitialEventInstallationPlan::Install {
            admitted_revision: SessionRevision(1),
            wake_activity_epoch: 1,
        }
    );
    for rejected in [
        initial_event_installation_plan(SessionRevision(1), 0, true, true),
        initial_event_installation_plan(SessionRevision(0), 1, true, true),
        initial_event_installation_plan(SessionRevision(0), 0, false, true),
        initial_event_installation_plan(SessionRevision(0), 0, true, false),
    ] {
        assert_eq!(rejected, InitialEventInstallationPlan::Reject);
    }
}

/// The durable, adapter-side configuration of one Managed session, keyed by its
/// id (which is also its thread id). Everything here is what the wire `Session`
/// object needs beyond the runtime's committed transcript.
#[derive(Clone, Debug, PartialEq, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PersistedSession {
    pub session_id: crate::SessionId,
    /// Lifetime-retained immutable history; old roots remain unrecorded.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) update_history: Option<Box<crate::SessionUpdateHistory>>,
    /// Initial accepted presentation fact. Execution uses the original root fields.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) initial_agent_configuration:
        Option<Box<awaken_session_configuration_contract::SessionAgentConfiguration>>,
    /// Durable creation and last root-mutation times. Absent means historical
    /// unknown, never the current read/recovery time. Omission preserves legacy
    /// aggregate fingerprints until the owning application commits a new fact.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub created_at_unix_ms: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub updated_at_unix_ms: Option<u64>,
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
    /// Original named admissions survive activity settlement and terminal fencing.
    /// Missing history is legacy unknown; a fresh empty history is not a full
    /// execution-domain or physical-closure proof.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) run_activities: Option<Box<super::activity_state::SessionRunActivities>>,
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
    /// Absent is historical unknown. Only a fresh constructor establishes the
    /// explicit empty fact before the original atomic root insert.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) environment_materialization: Option<Box<crate::SessionEnvironmentMaterialization>>,
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
    pub(crate) realization_progress: crate::SessionRealizationProgress,
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
    /// Stored retry observations. Assignment commands own their changes.
    ///
    /// Root boundary decision table: C1 a caller owns/borrows a snapshot;
    /// C2 it requests a shared read, whole replacement, or mutable access.
    /// E1 shared observations remain available; E2 both writes are rejected.
    /// Historical root decoding remains the original import boundary and does
    /// not grant a repository mutation or predecessor-validation capability.
    /// ```
    /// use awaken_session_state_contract::{PersistedSession, SessionRealizationProgress};
    /// fn inspect(session: &PersistedSession) -> &SessionRealizationProgress {
    ///     session.realization_progress()
    /// }
    /// ```
    /// ```compile_fail,E0616
    /// # use awaken_session_state_contract::PersistedSession;
    /// fn reset(session: &mut PersistedSession) {
    ///     session.realization_progress = Default::default();
    /// }
    /// ```
    /// ```compile_fail,E0308
    /// # use awaken_session_state_contract::{PersistedSession, SessionRealizationProgress};
    /// fn mutate(session: &mut PersistedSession) {
    ///     let _: &mut SessionRealizationProgress = session.realization_progress();
    /// }
    /// ```
    #[must_use]
    pub const fn realization_progress(&self) -> &crate::SessionRealizationProgress {
        &self.realization_progress
    }

    /// Immutable first-effect provenance; absence is historical unknown.
    #[must_use]
    pub fn environment_materialization(&self) -> Option<&crate::SessionEnvironmentMaterialization> {
        self.environment_materialization.as_deref()
    }

    /// Construct a complete Session root after the creation compiler has
    /// consumed every authoring input. New creation persists this shape in one
    /// insert. Historical interrupted rows remain representable only through
    /// deserialization; no creation API can produce another one.
    #[must_use]
    #[allow(clippy::too_many_arguments)]
    pub fn frozen_with_budget(
        session_id: impl Into<crate::SessionId>,
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
            update_history: Some(Box::new(
                crate::update_history::SessionUpdateHistory::fresh(),
            )),
            initial_agent_configuration: None,
            created_at_unix_ms: None,
            updated_at_unix_ms: None,
            revision: SessionRevision::default(),
            baseline: crate::SessionBaselineState::Frozen(baseline),
            title,
            metadata,
            tools,
            event_batches: Vec::new(),
            activity_epoch: 0,
            active_activity_epochs: BTreeSet::new(),
            run_activities: Some(Box::new(
                super::activity_state::SessionRunActivities::fresh(),
            )),
            running_interval: None,
            closed_runtime_intervals: Vec::new(),
            runtime_active_millis: 0,
            usage_cursor: Default::default(),
            budget,
            environment: Default::default(),
            environment_materialization: Some(Box::new(
                crate::SessionEnvironmentMaterialization::fresh(),
            )),
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
        let InitialEventInstallationPlan::Install {
            admitted_revision,
            wake_activity_epoch,
        } = initial_event_installation_plan(
            self.revision,
            self.activity_epoch,
            self.event_batches.is_empty(),
            self.active_activity_epochs.is_empty(),
        )
        else {
            return Err(crate::SessionEventBatchError::ProgressMismatch);
        };
        // Complete and validate the detached batch before changing any root
        // field. A malformed public plan cannot leave a partial activity epoch.
        plan.batch.admitted_revision = admitted_revision;
        plan.batch
            .anchor_admission_projection(crate::SessionEventProjectionAnchor {
                source_commit_cursor: 0,
            })?;
        plan.batch.wake_activity_epoch = Some(wake_activity_epoch);
        // This wake only drives the validated batch committed beside it. User
        // and exact Reply lowering retain their named admissions before effect;
        // the batch itself remains the sole other Event-command provenance.
        if !self.activity_state().begin_activity_epoch_at(
            wake_activity_epoch,
            super::activity_state::ActivityAdmissionSource::RegisteredEventWake,
        ) {
            return Err(crate::SessionEventBatchError::ProgressMismatch);
        }
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
        self.activity_state().transition_execution(next)
    }

    /// Advance the monotonic activity fence and record the newly admitted
    /// activity. An empty set on a historical Running row is deliberately not
    /// backfilled here: after crash recovery the predecessor has no durable
    /// completion owner, so the successor becomes the sole explicit activity.
    pub fn begin_activity_epoch(&mut self) -> Option<u64> {
        self.activity_state().begin_activity_epoch()
    }

    /// Open an activity at the exact root revision reserved by an idempotent
    /// application mutation. The revision is monotonic and therefore remains in
    /// the same fencing domain as ordinary activity epochs without an auxiliary
    /// operation-to-epoch registry.
    pub fn begin_activity_epoch_at(
        &mut self,
        epoch: u64,
        source: Option<(
            &str,
            &awaken_agent_contract::ThreadId,
            &awaken_agent_contract::RunId,
        )>,
    ) -> bool {
        let source = match source {
            Some((operation, thread, run)) => {
                super::activity_state::ActivityAdmissionSource::Named(operation, thread, run)
            }
            None => super::activity_state::ActivityAdmissionSource::Anonymous,
        };
        self.activity_state().begin_activity_epoch_at(epoch, source)
    }

    /// Settle one admitted activity epoch.
    ///
    /// `None` is an unknown, duplicate, or stale completion. `Some(false)`
    /// means another admitted activity remains; `Some(true)` means this was the
    /// last activity and the application may close the shared Running interval.
    /// A historical Running row with no explicit set treats its non-zero scalar
    /// epoch as a singleton for backward-compatible settlement.
    pub fn settle_activity_epoch(&mut self, expected_epoch: u64) -> Option<bool> {
        self.activity_state().settle_activity_epoch(expected_epoch)
    }

    fn activity_state(&mut self) -> super::activity_state::ActivityState<'_> {
        super::activity_state::ActivityState {
            epoch: &mut self.activity_epoch,
            active: &mut self.active_activity_epochs,
            execution: &mut self.execution,
            history: &mut self.run_activities,
        }
    }

    /// Read-only raw facts. The flag means some admitted activity had no raw
    /// Run identity; neither its absence nor an empty slice proves full closure.
    pub fn run_activities(&self) -> Option<(&[super::SessionRunActivityAdmissionRecord], bool)> {
        self.run_activities
            .as_deref()
            .map(|history| (history.records.as_slice(), history.unrecorded))
    }

    /// Replay never repairs missing history. Only an already retained named
    /// operation can reject different raw coordinates under the same receipt.
    pub fn validate_run_activity_replay(
        &self,
        operation: &str,
        epoch: impl TryInto<awaken_lease_contract::fence::Epoch>,
        source: Option<(
            &awaken_agent_contract::ThreadId,
            &awaken_agent_contract::RunId,
        )>,
    ) -> Result<(), super::SessionMutationValidationError> {
        let epoch = epoch
            .try_into()
            .map_err(|_| super::SessionMutationValidationError::ActivityHistoryMismatch)?;
        if let Some(history) = self.run_activities.as_deref()
            && let Some(record) = history
                .records
                .iter()
                .find(|record| record.operation_id == operation)
            && (record.activity_epoch != epoch
                || source.is_some_and(|(thread, run)| {
                    record.thread_id != *thread || record.run_id != *run
                }))
        {
            return Err(super::SessionMutationValidationError::ActivityHistoryMismatch);
        }
        Ok(())
    }

    /// Join one successfully applied Dispatch settlement to its exact retained
    /// User Event and activity receipt. The caller commits the returned root;
    /// therefore activity removal, receipt publication and processed visibility
    /// cannot be split across repository revisions.
    pub fn settle_user_event_dispatch(
        &mut self,
        run_id: &awaken_agent_contract::agent::run::Id,
        boundary: crate::SessionAgentRunBoundary,
        expected_epoch: u64,
        projection_anchor: SessionEventProjectionAnchor,
        processed_at_unix_ms: u64,
    ) -> Result<SessionEventDispatchSettlementEffect, SessionEventBatchError> {
        let coordinate = self
            .user_run_coordinate(run_id)?
            .map(|(batch_index, operation_id)| (batch_index, operation_id.to_string()));
        let Some((batch_index, operation_id)) = coordinate else {
            return Ok(SessionEventDispatchSettlementEffect::NotEvent);
        };
        let active = self.active_activity_epochs.contains(&expected_epoch);
        let receipt = SessionEventDispatchSettlementReceipt {
            session_activity_epoch: expected_epoch,
            commit_observation_anchor: projection_anchor,
        };
        let plan = self.event_batches[batch_index].record_user_run_dispatch_settlement(
            &operation_id,
            run_id,
            active,
            receipt,
            processed_at_unix_ms,
        )?;
        let inputs_allow_idle = session_inputs_allow_idle(&self.event_batches, boundary);
        match plan {
            SessionEventDispatchSettlementPlan::Apply => {
                let last_active = self
                    .settle_activity_epoch(expected_epoch)
                    .ok_or(SessionEventBatchError::ProgressMismatch)?;
                Ok(SessionEventDispatchSettlementEffect::Applied {
                    fully_settled: session_event_execution_may_close(
                        plan,
                        true,
                        last_active,
                        inputs_allow_idle,
                    ),
                })
            }
            SessionEventDispatchSettlementPlan::Replay => {
                // Old binaries could persist an exact settled Awaiting receipt
                // but retain Running because a later User was queued. Recheck
                // only that same interval/epoch; never retire a newer activity.
                Ok(SessionEventDispatchSettlementEffect::Replayed {
                    fully_settled: boundary == crate::SessionAgentRunBoundary::Awaiting
                        && inputs_allow_idle
                        && self.active_activity_epochs.is_empty()
                        && self.activity_epoch == expected_epoch
                        && self.execution == SessionExecutionState::Running
                        && self.running_interval.as_ref().is_some_and(|interval| {
                            interval.observations.iter().any(|observation| {
                                observation.activity_epoch == expected_epoch
                                    && observation.run_id == *run_id
                                    && observation.thread_id.0 == self.session_id
                                    && observation.source_commit_cursor
                                        == projection_anchor.source_commit_cursor
                            })
                        }),
                })
            }
            SessionEventDispatchSettlementPlan::Reject => {
                Err(SessionEventBatchError::ProgressMismatch)
            }
        }
    }

    /// Resolve the original HTTP retry identity retained with one exact User
    /// Run. Create-time inputs legitimately return `None`; multiple retained
    /// coordinates are corrupt and fail closed through the same lookup used by
    /// Dispatch settlement.
    pub fn user_run_idempotency_key(
        &self,
        run_id: &awaken_agent_contract::agent::run::Id,
    ) -> Result<Option<&str>, SessionEventBatchError> {
        let Some((batch_index, _)) = self.user_run_coordinate(run_id)? else {
            return Ok(None);
        };
        Ok(self.event_batches[batch_index].idempotency_key.as_deref())
    }

    fn user_run_coordinate(
        &self,
        run_id: &awaken_agent_contract::agent::run::Id,
    ) -> Result<Option<(usize, &str)>, SessionEventBatchError> {
        let mut coordinate = None;
        for (batch_index, batch) in self.event_batches.iter().enumerate() {
            if let Some(operation_id) = batch.user_run_operation_id(run_id)? {
                if coordinate.is_some() {
                    return Err(SessionEventBatchError::ProgressMismatch);
                }
                coordinate = Some((batch_index, operation_id));
            }
        }
        Ok(coordinate)
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
        archived_at_unix_ms: u64,
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
            archived_at_unix_ms: Some(archived_at_unix_ms),
        };
        Ok(true)
    }

    /// Compile and apply the hidden deletion phase before any external cleanup.
    /// The returned optional fact is the application's sole lifecycle-fact
    /// source; Replay returns `None` and makes no cleanup request. Archived and
    /// activation-failed Sessions remain deletable because disposition is an
    /// orthogonal state axis.
    pub fn request_delete(
        &mut self,
        owner_scope: &str,
        requested_at_unix_s: i64,
    ) -> Result<Option<crate::ManagedLifecycleFact>, crate::SessionCleanupError> {
        let session_id = self.session_id.clone();
        let plan = session_delete_request_plan(
            SessionDeleteDispositionClass::from(&self.disposition),
            self.execution.is_terminal(),
            &session_id,
            owner_scope,
            requested_at_unix_s,
        );
        let Some((terminalize_execution, cleanup_identity)) = plan.commit_parts() else {
            return Ok(None);
        };
        self.terminal_cleanup
            .request_exact(&session_id, &cleanup_identity)?;
        if terminalize_execution {
            self.execution = SessionExecutionState::Terminated;
        }
        self.active_activity_epochs.clear();
        self.disposition = SessionDisposition::Deleting;
        Ok(plan.into_lifecycle_fact())
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
            .has_complete_legacy_receipts(self.session_id.as_str())
    }

    /// Normalize one complete historical receipt set and retire the Resource
    /// projection atomically. Current callers cannot supply or manufacture
    /// one-stage evidence through this compatibility boundary. The
    /// Environment retires only as `LegacyCleanupNormalized` decides: a
    /// terminal root, fenced or never leased; Restoring is refused because
    /// historical evidence predates its exact physical target, so both
    /// authorities stay with the current two-stage reconciler.
    pub fn normalize_legacy_terminal_cleanup(
        &mut self,
        release_reason: impl Into<String>,
    ) -> Result<bool, crate::SessionCleanupError> {
        let retirement = decide_terminal_cleanup(
            self,
            crate::SessionEnvironmentEvent::LegacyCleanupNormalized,
        )?;
        let changed = self
            .terminal_cleanup
            .normalize_legacy_completion(self.session_id.as_str())?;
        if changed {
            self.resources.complete_terminal_release(release_reason);
            if retirement.next() == crate::SessionEnvironmentStep::Unmaterialized {
                self.environment = crate::SessionEnvironmentState::Unmaterialized;
            }
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
            decide_terminal_cleanup(
                self,
                crate::SessionEnvironmentEvent::CleanedUp {
                    already_recorded: true,
                },
            )?;
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
        let retirement = decide_terminal_cleanup(
            self,
            crate::SessionEnvironmentEvent::CleanedUp {
                already_recorded: false,
            },
        )?;
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
            if retirement.next() == crate::SessionEnvironmentStep::Unmaterialized {
                self.environment = crate::SessionEnvironmentState::Unmaterialized;
            }
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
        decide_terminal_cleanup(self, crate::SessionEnvironmentEvent::RetireForCleanup)?;
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
        decide_terminal_cleanup(self, crate::SessionEnvironmentEvent::RetireForCleanup)?;
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
            return Err(crate::SessionEnvironmentReceiptError::Rejected(
                crate::SessionEnvironmentRejection::Terminal,
            ));
        }
        let crate::SessionEnvironmentState::Suspending {
            operation: current_operation,
            suspend_phase: crate::SuspendPhase::ReadyToDispose,
            checkpoint: Some(_),
            ..
        } = &self.environment
        else {
            return Err(crate::SessionEnvironmentReceiptError::Rejected(
                crate::SessionEnvironmentRejection::WrongStep,
            ));
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
            return Err(crate::SessionEnvironmentReceiptError::Rejected(
                crate::SessionEnvironmentRejection::Terminal,
            ));
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
    /// The Environment decision admits the step (active, fenced, consistent
    /// Suspending evidence) before the exact lease and receipt laws run.
    pub fn record_source_release_prepared(
        &mut self,
        receipt: &crate::SourceReleasePreparedReceipt,
        now_unix_ms: u64,
    ) -> Result<bool, crate::SessionEnvironmentReceiptError> {
        crate::transition_environment(
            crate::SessionEnvironmentFacts::project(self, now_unix_ms, None),
            crate::SessionEnvironmentEvent::SourceReleasePrepared,
        )?;
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
                crate::SessionEnvironmentTransitionError::Rejected(rejection) => rejection.into(),
                crate::SessionEnvironmentTransitionError::ReceiptMismatch => {
                    crate::SessionEnvironmentReceiptError::Mismatch
                }
                crate::SessionEnvironmentTransitionError::CheckpointExpired
                | crate::SessionEnvironmentTransitionError::InvalidCheckpointPolicy(_) => {
                    crate::SessionEnvironmentReceiptError::Rejected(
                        crate::SessionEnvironmentRejection::InvalidTransition,
                    )
                }
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
            return Err(crate::SessionEnvironmentReceiptError::Rejected(
                crate::SessionEnvironmentRejection::Terminal,
            ));
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
        session_is_terminal(self.execution, &self.disposition)
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

    #[must_use]
    pub const fn archived_at_unix_ms(&self) -> Option<u64> {
        self.disposition.archived_at_unix_ms()
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
    pub fn needs_initial_realization_recovery(&self) -> bool {
        self.frozen_baseline().is_some() && self.execution.requires_initial_realization_recovery()
    }

    #[must_use]
    pub fn needs_reconciliation(&self) -> bool {
        self.needs_initial_realization_recovery()
            || self.needs_event_reconciliation()
            || self.needs_outcome_reconciliation()
            || self.needs_resource_reconciliation()
            || self.resources.has_references()
            || self.mcp.needs_reconciliation()
            || !matches!(
                self.environment,
                crate::SessionEnvironmentState::Unmaterialized
            )
            || self.verified_cleanup_needs_reconciliation()
            || self.has_unfinished_worker_realization().unwrap_or(true)
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
    /// work exists. A live Session activity owns the Thread until its committed
    /// boundary is observed; driving the Outcome concurrently would replay the
    /// same stable Worker Run while its resume is still in flight. This trades a
    /// bounded read for one source of effect truth. A prepared initial Outcome
    /// may also drive the first Run while physical realization is pending: the
    /// existing Run admission/Worker path owns that realization. Requiring Idle
    /// here would wait for a Run that this very continuation must first create.
    #[must_use]
    pub fn needs_outcome_reconciliation(&self) -> bool {
        !self.is_terminal()
            && !self.has_active_activities()
            && (self.execution == crate::SessionExecutionState::Idle
                || self.execution.requires_initial_realization_recovery())
            && self.event_batches.iter().any(|batch| {
                batch.events.iter().any(|entry| {
                    entry.processed
                        && matches!(
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
        if !terminal_environment_receipt_admitted(
            self.is_terminal(),
            self.terminal_cleanup.is_fenced(),
        ) {
            return Err(crate::SessionEnvironmentReceiptError::Rejected(
                crate::SessionEnvironmentRejection::Terminal,
            ));
        }
        receipt.verify()?;
        let intent = receipt.effect_intent()?;
        if let Some(facts) = self.environment_materialization.as_deref() {
            facts.validate_existing_origin_effect(&intent)?;
        }
        crate::transition_environment(
            crate::SessionEnvironmentFacts::project(self, now_unix_ms, intent.realization()),
            crate::SessionEnvironmentEvent::Materialized {
                kind: intent.kind().materialization_kind(),
                already_applied: self.environment.records_effect(intent.effect_id()),
            },
        )?;
        let authorization = self.authorize_environment_effect(owner_scope, &intent, now_unix_ms)?;
        if let Some(facts) = self.environment_materialization.as_deref() {
            facts.validate_receipt(&intent)?;
        }
        let changed = self
            .environment
            .apply_authorized_receipt(receipt, authorization)?;
        if changed && let Some(facts) = self.environment_materialization.as_deref_mut() {
            facts.record_receipt(&intent);
        }
        Ok(changed)
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

    /// Project only a published, live Worker-held MCP generation from this
    /// aggregate. Transport adapters may route its opaque capability, but
    /// cannot select a Worker or revive a stale generation from desired state.
    #[must_use]
    pub fn current_worker_relay_generation(
        &self,
        attachment_id: &awaken_session_configuration_contract::mcp::McpAttachmentId,
        version: u64,
        now_unix_ms: u64,
    ) -> Option<(
        String,
        awaken_session_configuration_contract::mcp::McpGenerationRef,
    )> {
        if self.is_terminal() || self.is_hidden() {
            return None;
        }
        let current = self.realization.as_ref()?;
        if current.owner.trim().is_empty() {
            return None;
        }
        let mut matches = self.mcp.visible().into_iter().filter(|attachment| {
            &attachment.attachment_id == attachment_id && attachment.generation.version() == version
        });
        let attachment = matches.next()?;
        if matches.next().is_some()
            || !attachment.publication_acknowledged
            || attachment.credential.is_none()
            || !attachment
                .selected_plaintext_holder
                .as_ref()
                .is_some_and(|holder| {
                    holder.boundary == awaken_credential_contract::PlaintextBoundary::Worker
                })
        {
            return None;
        }
        let claim = attachment.realization.as_ref()?;
        let asserted = SessionRealizationLease {
            owner: current.owner.clone(),
            runtime_incarnation: claim.runtime_incarnation.clone(),
            epoch: claim.lease_epoch,
            expires_at_unix_ms: claim.lease_expires_at_unix_ms,
        };
        if !crate::realization_lease_authorizes(current, &asserted, now_unix_ms) {
            return None;
        }
        let generation = awaken_session_configuration_contract::mcp::McpGenerationRef::from_claim(
            &self.session_id,
            &attachment.generation,
            claim,
        )?;
        Some((current.owner.clone(), generation))
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

/// Decide one terminal cleanup event (`RetireForCleanup`, `CleanedUp`,
/// `LegacyCleanupNormalized`) through the Environment transition function. The event asserts no lease:
/// callers have already checked the exact lease generation, so the root's own
/// lease stands.
fn decide_terminal_cleanup(
    session: &PersistedSession,
    event: crate::SessionEnvironmentEvent,
) -> Result<crate::SessionEnvironmentDecision, crate::SessionCleanupError> {
    crate::transition_environment(
        crate::SessionEnvironmentFacts::project_terminal_cleanup(session),
        event,
    )
    .map_err(crate::SessionCleanupError::from)
}

#[cfg(test)]
mod worker_relay_route_tests {
    use super::*;
    use crate::session_repo::mutation_tests::session;
    use awaken_credential_contract::{
        CredentialAccess, CredentialExecutionPolicy, CredentialMaterialSource, CredentialRef,
        CredentialUsage, ModelExposurePolicy, PlaintextBoundary, PlaintextHolder,
    };
    use awaken_session_configuration_contract::mcp::{
        McpAttachmentDraft, McpAttachmentId, McpAttachmentOrigin, McpAttachmentState,
        McpRealizationClaim, SessionMcpAttachmentSet,
    };

    #[test]
    fn only_the_published_live_worker_generation_has_a_route_owner() {
        // Causes: C1 one active and acknowledged Worker-held generation;
        // C2 exact attachment/version; C3 its claim is a live renewal of the
        // Session root lease; C4 credential holder is Worker. Effects: E1 one
        // exact owner and generation; E2 every missing/mismatched/stale cause
        // yields no routable target. The old RunDispatch claim is not a second
        // source of Session-realization authority.
        // | Rule | C1 | C2 | C3 | C4 | Effect |
        // | R1 | yes | yes | yes | yes | E1 |
        // | R2 | no | any | any | any | E2 |
        // | R3 | yes | no | any | any | E2 |
        // | R4 | yes | yes | no | any | E2 |
        // | R5 | yes | yes | yes | no | E2 |
        let mut root = session("relay-route", crate::SessionRevision(1));
        let holder = PlaintextHolder::new(PlaintextBoundary::Worker, "worker.example");
        let credential = CredentialAccess::new(
            CredentialRef {
                id: "credential-1".into(),
                revision: 1,
            },
            CredentialMaterialSource::ControlPlaneReference,
            CredentialUsage::HttpHeader {
                name: "authorization".into(),
                scheme: Some("Bearer".into()),
            },
            CredentialExecutionPolicy::exact(holder.clone(), ModelExposurePolicy::VirtualOnly),
        );
        root.mcp = SessionMcpAttachmentSet::from_initial(
            vec![McpAttachmentDraft {
                name: "calc".into(),
                target: awaken_agent_contract::McpTarget::parse_http("https://mcp.example.test/")
                    .unwrap(),
                prompts_as_skills: false,
                credential: Some(credential),
                origin: McpAttachmentOrigin::Agent,
            }],
            Some(holder),
        )
        .unwrap();
        let attachment = &mut root.mcp.attachments[0];
        attachment.state = McpAttachmentState::Active;
        attachment.publication_acknowledged = true;
        attachment.realization = Some(McpRealizationClaim {
            realization_id: "realization-1".into(),
            runtime_incarnation: "runtime-1".into(),
            lease_epoch: 3,
            lease_expires_at_unix_ms: 100,
            stage_idempotency_key: "stage-1".into(),
        });
        root.realization = Some(SessionRealizationLease {
            owner: "k8s:ns:worker:pod-uid".into(),
            runtime_incarnation: "runtime-1".into(),
            epoch: 3,
            expires_at_unix_ms: 110,
        });
        let id = root.mcp.attachments[0].attachment_id.clone();
        let (owner, generation) = root
            .current_worker_relay_generation(&id, 1, 99)
            .expect("R1 exact active route");
        assert_eq!(owner, "k8s:ns:worker:pod-uid", "R1/E1");
        assert_eq!(generation.generation().version(), 1, "R1/E1");

        assert!(
            root.current_worker_relay_generation(&McpAttachmentId("other".into()), 1, 99)
                .is_none(),
            "R3/E2 foreign attachment"
        );
        assert!(
            root.current_worker_relay_generation(&id, 2, 99).is_none(),
            "R3/E2 foreign version"
        );
        assert!(
            root.current_worker_relay_generation(&id, 1, 110).is_none(),
            "R4/E2 exact expiry"
        );
        let mut changed = root.clone();
        changed.mcp.attachments[0].publication_acknowledged = false;
        assert!(
            changed
                .current_worker_relay_generation(&id, 1, 99)
                .is_none(),
            "R2/E2"
        );
        changed = root.clone();
        changed.mcp.attachments[0]
            .realization
            .as_mut()
            .unwrap()
            .lease_epoch = 4;
        assert!(
            changed
                .current_worker_relay_generation(&id, 1, 99)
                .is_none(),
            "R4/E2"
        );
        changed = root.clone();
        changed.mcp.attachments[0]
            .selected_plaintext_holder
            .as_mut()
            .unwrap()
            .boundary = PlaintextBoundary::Platform;
        assert!(
            changed
                .current_worker_relay_generation(&id, 1, 99)
                .is_none(),
            "R5/E2"
        );
    }
}
