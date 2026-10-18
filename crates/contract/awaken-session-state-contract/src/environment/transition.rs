// @formal entrypoint session_environment.terminal_session_never_reenters_a_live_environment
// @formal entrypoint session_environment.resident_is_entered_only_by_materialization_or_restore
// @formal entrypoint session_environment.effect_instructions_require_a_current_realization_lease
//! The Session Environment transition function (ADR-0089 D1, ADR-0090 D3).
//!
//! [`transition_environment`] is the one pure decision over the Environment
//! sub-state: given the aggregate's [`SessionEnvironmentFacts`] and one
//! command or observation it returns the next lifecycle step plus the effect
//! instruction the realization executor must carry out, or a closed
//! rejection. It reads no clock (time arrives already folded into the facts by
//! [`SessionEnvironmentFacts::project`]) and performs no I/O.
//!
//! The data-carrying laws on [`SessionEnvironmentState`] (receipt identity,
//! operation ids, checkpoints, generations) still build the exact durable
//! value; this function owns which step may follow which, and which effect a
//! step demands. Every rule below is derived from an existing production path
//! and names it.

use super::{SessionEnvironmentPhase, SessionEnvironmentState, SuspendPhase};

/// The durable lifecycle step of a Session Environment: its phase, refined by
/// the suspend sub-phase. This is the state the transition function reasons
/// over; [`SessionEnvironmentState`] carries the step's exact data.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SessionEnvironmentStep {
    Unmaterialized,
    Resident,
    Suspending(SuspendPhase),
    Hibernated,
    Restoring,
}

impl SessionEnvironmentStep {
    /// The durable phase this step refines: the one source of the phase a
    /// global count selects.
    #[must_use]
    pub const fn phase(self) -> SessionEnvironmentPhase {
        match self {
            Self::Unmaterialized => SessionEnvironmentPhase::Unmaterialized,
            Self::Resident => SessionEnvironmentPhase::Resident,
            Self::Suspending(_) => SessionEnvironmentPhase::Suspending,
            Self::Hibernated => SessionEnvironmentPhase::Hibernated,
            Self::Restoring => SessionEnvironmentPhase::Restoring,
        }
    }

    /// Whether realization effects (lease assignment, staging, MCP publication)
    /// may run. A continuation has closed process admission until restore
    /// commits Resident again.
    #[must_use]
    pub const fn admits_realization_effects(self) -> bool {
        matches!(self, Self::Unmaterialized | Self::Resident)
    }
}

impl SessionEnvironmentState {
    /// Project the durable value onto its lifecycle step.
    #[must_use]
    pub const fn step(&self) -> SessionEnvironmentStep {
        match self {
            Self::Unmaterialized => SessionEnvironmentStep::Unmaterialized,
            Self::Resident { .. } => SessionEnvironmentStep::Resident,
            Self::Suspending { suspend_phase, .. } => {
                SessionEnvironmentStep::Suspending(*suspend_phase)
            }
            Self::Hibernated { .. } => SessionEnvironmentStep::Hibernated,
            Self::Restoring { .. } => SessionEnvironmentStep::Restoring,
        }
    }

    /// Whether the Resident binding already records this effect: the replay
    /// of its own materialization.
    #[must_use]
    pub fn records_effect(&self, effect_id: &str) -> bool {
        matches!(self, Self::Resident { effect_id: Some(recorded), .. } if recorded == effect_id)
    }

    /// The one realization-effect gate, shared by scan, direct realization
    /// entry and the transition function.
    #[must_use]
    pub const fn admits_realization_effects(&self) -> bool {
        self.step().admits_realization_effects()
    }

    /// Whether a Suspending value carries exactly the evidence its sub-phase
    /// requires: no checkpoint or preparation before upload completes, a
    /// checkpoint from ReadyToDispose, and the durable preparation receipt in
    /// Disposing. Every other step is trivially consistent.
    #[must_use]
    pub const fn suspend_evidence_is_consistent(&self) -> bool {
        match self {
            Self::Suspending {
                suspend_phase,
                checkpoint,
                source_release_preparation,
                ..
            } => match suspend_phase {
                SuspendPhase::Quiescing | SuspendPhase::Uploading => {
                    checkpoint.is_none() && source_release_preparation.is_none()
                }
                SuspendPhase::ReadyToDispose => {
                    checkpoint.is_some() && source_release_preparation.is_none()
                }
                SuspendPhase::Disposing => {
                    checkpoint.is_some() && source_release_preparation.is_some()
                }
            },
            _ => true,
        }
    }
}

/// Whether the Session still admits Environment work, and in which terminal
/// sub-state. `CleanupJoining` is the terminal cleanup's Fenced join, the only
/// terminal window in which an already-started Environment receipt may land.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SessionTerminality {
    Active,
    CleanupJoining,
    Terminal,
}

/// The realization lease as the event sees it.
///
/// `Fenced`: the root holds a lease and the event's asserted lease (if any) is
/// a live renewal of it. `Unfenced`: neither the root nor the event carries a
/// lease (legacy and Coordinator-local Environments). `Stale`: the event
/// asserts a lease the root no longer authorizes, or presence disagrees.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RealizationLeaseStanding {
    Fenced,
    Unfenced,
    Stale,
}

/// Every aggregate fact the transition function reads. Time-dependent facts
/// are already evaluated at the caller-supplied `now`. Only
/// [`SessionEnvironmentFacts::project`] constructs it, so no caller
/// can decide over facts the aggregate did not project.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SessionEnvironmentFacts {
    pub(crate) step: SessionEnvironmentStep,
    pub(crate) terminality: SessionTerminality,
    pub(crate) execution_idle: bool,
    pub(crate) activities_open: bool,
    pub(crate) lease: RealizationLeaseStanding,
    /// Resident only: the binding carries its effect id and generation.
    pub(crate) resident_generated: bool,
    /// Resident only: a generated binding has an idle edge, the frozen policy
    /// is checkpoint-and-release, and `now` reached the checkpoint deadline.
    pub(crate) suspend_due: bool,
    /// Suspending only: [`SessionEnvironmentState::suspend_evidence_is_consistent`].
    pub(crate) suspend_evidence_consistent: bool,
    /// The step's generation is unexpired at `now` (true without a generation).
    pub(crate) generation_live: bool,
    /// Hibernated only: the checkpoint is unexpired at `now`.
    pub(crate) checkpoint_live: bool,
    /// The frozen topology has a continuation transport: false only for an
    /// external Worker placement with checkpoint-and-release retention
    /// ([`crate::SessionPlacementClass::supports_idle_retention`]); true
    /// without a frozen baseline.
    pub(crate) continuation_topology_supported: bool,
    /// Resident only: the binding's Memory evidence admits a suspend (no
    /// pending Memory generation, and the active Memory inputs join the
    /// handle's evidence without a writable legacy copy).
    pub(crate) suspend_memory_admitted: bool,
}

/// How a materialization effect relates the environment to its binding.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MaterializationKind {
    Create,
    Adopt,
    Rebuild,
    ReserveResources,
}

/// A command (a use case asking for Environment work) or an observation (the
/// realization executor reporting what an effect did).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SessionEnvironmentEvent {
    /// Begin a realization: assign the lease and stage the Session.
    BeginRealization,
    /// Extend the current realization lease.
    RenewRealization,
    /// Admit one materialization effect before provider I/O.
    /// `already_applied`: the Resident binding already records this effect.
    AdmitMaterialization {
        kind: MaterializationKind,
        already_applied: bool,
    },
    /// Lifecycle supervisor: advance a due or unfinished continuation.
    AdvanceContinuation,
    /// Driving ingress needs the Environment resident before Runtime I/O.
    RequireResident,
    /// Terminal cleanup asks for its next retirement step.
    RetireForCleanup,
    /// A materialization receipt (create/adopt/rebuild/reservation).
    Materialized {
        kind: MaterializationKind,
        already_applied: bool,
    },
    /// A claim delivered a legacy raw binding for an Unmaterialized projection.
    LegacyBindingDelivered,
    Quiesced,
    Checkpointed,
    SourceReleasePrepared,
    /// The checkpoint source was disposed (released).
    SourceReleased,
    Restored,
    ExpiredCheckpointDeleted,
    /// Terminal cleanup disposed the Environment. `already_recorded` is the
    /// exact completed replay, which needs no lease.
    CleanedUp {
        already_recorded: bool,
    },
    /// An effect failed. A failure never proves physical absence (ADR-0074),
    /// so its disposition decides only the lease and execution axes.
    EffectFailed,
}

/// The effect the realization executor must perform next.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SessionEnvironmentInstruction {
    Materialize(MaterializationKind),
    Quiesce,
    Checkpoint,
    PrepareSourceRelease,
    DisposeSource,
    Restore,
    DeleteExpiredCheckpoint,
    Retire,
}

impl SessionEnvironmentInstruction {
    /// Instructions that create, destroy or retire physical state: the
    /// aggregate must hold a current lease, never a legacy unfenced one.
    #[must_use]
    pub const fn requires_fenced_lease(self) -> bool {
        matches!(
            self,
            Self::Materialize(MaterializationKind::Create)
                | Self::PrepareSourceRelease
                | Self::DisposeSource
                | Self::Retire
        )
    }
}

/// The step after the event, and the effect it demands. `next == step` is an
/// idempotent replay or an instruction re-issued for the same step.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SessionEnvironmentDecision {
    next: SessionEnvironmentStep,
    instruction: Option<SessionEnvironmentInstruction>,
}

impl SessionEnvironmentDecision {
    /// The step after the event.
    #[must_use]
    pub const fn next(&self) -> SessionEnvironmentStep {
        self.next
    }

    /// The effect the realization executor must carry out, if any.
    #[must_use]
    pub const fn instruction(&self) -> Option<SessionEnvironmentInstruction> {
        self.instruction
    }
}

/// Why the aggregate refuses an event. Closed: callers map it once. The
/// refusals that receipts also report keep the receipt wording callers read.
#[derive(Clone, Copy, Debug, PartialEq, Eq, thiserror::Error)]
pub enum SessionEnvironmentRejection {
    #[error("the Session is terminal")]
    Terminal,
    #[error("Environment retirement requires a terminal Session")]
    NotTerminal,
    #[error("a continuation has closed realization effects")]
    ContinuationInProgress,
    #[error("open activities forbid suspending or restoring the Environment")]
    ActivitiesOpen,
    #[error("the resident binding has no effect id and generation to suspend")]
    NotGenerated,
    #[error("the event is not valid in the current Environment step")]
    WrongStep,
    #[error("Session environment receipt requires a resident source binding")]
    RequiresResident,
    #[error("Session environment receipt does not prove an allowed state transition")]
    InvalidTransition,
    #[error("the realization lease is not current")]
    LeaseNotCurrent,
    #[error("the Environment generation expired before its checkpoint")]
    GenerationExpired,
    #[error("the checkpoint is still live")]
    CheckpointLive,
    #[error("the suspend evidence does not match its sub-phase")]
    InconsistentEvidence,
    #[error("Session materialization remains pending its exact original effect and source closure")]
    MaterializationPending,
    #[error(
        "checkpoint-and-release retention has no continuation transport for an external Worker"
    )]
    UnsupportedContinuationTopology,
    #[error("the Memory evidence of the resident binding does not admit a suspend")]
    MemoryEvidenceUnproven,
}

type Outcome = Result<SessionEnvironmentDecision, SessionEnvironmentRejection>;

const fn stay(facts: SessionEnvironmentFacts) -> Outcome {
    Ok(SessionEnvironmentDecision {
        next: facts.step,
        instruction: None,
    })
}

const fn enter(next: SessionEnvironmentStep) -> Outcome {
    Ok(SessionEnvironmentDecision {
        next,
        instruction: None,
    })
}

const fn instruct(
    next: SessionEnvironmentStep,
    instruction: SessionEnvironmentInstruction,
) -> Outcome {
    Ok(SessionEnvironmentDecision {
        next,
        instruction: Some(instruction),
    })
}

/// Decide one Environment event. Pure, total and exhaustively tested.
pub const fn transition_environment(
    facts: SessionEnvironmentFacts,
    event: SessionEnvironmentEvent,
) -> Outcome {
    use SessionEnvironmentEvent as E;
    match event {
        E::BeginRealization => begin_realization(facts),
        E::RenewRealization => renew_realization(facts),
        E::AdmitMaterialization {
            kind,
            already_applied,
        } => admit_materialization(facts, kind, already_applied),
        E::AdvanceContinuation => advance_continuation(facts),
        E::RequireResident => require_resident(facts),
        E::RetireForCleanup => retire_for_cleanup(facts),
        E::Materialized {
            kind,
            already_applied,
        } => materialized(facts, kind, already_applied),
        E::LegacyBindingDelivered => legacy_binding_delivered(facts),
        E::Quiesced => quiesced(facts),
        E::Checkpointed => checkpointed(facts),
        E::SourceReleasePrepared => source_release_prepared(facts),
        E::SourceReleased => source_released(facts),
        E::Restored => restored(facts),
        E::ExpiredCheckpointDeleted => expired_checkpoint_deleted(facts),
        E::CleanedUp { already_recorded } => cleaned_up(facts, already_recorded),
        E::EffectFailed => stay(facts),
    }
}

const fn is_active(facts: SessionEnvironmentFacts) -> bool {
    matches!(facts.terminality, SessionTerminality::Active)
}

const fn is_fenced(facts: SessionEnvironmentFacts) -> bool {
    matches!(facts.lease, RealizationLeaseStanding::Fenced)
}

const fn is_stale(facts: SessionEnvironmentFacts) -> bool {
    matches!(facts.lease, RealizationLeaseStanding::Stale)
}

/// session-application `session_for_realization`: terminal first, then the
/// realization-effect gate.
const fn begin_realization(facts: SessionEnvironmentFacts) -> Outcome {
    if !is_active(facts) {
        return Err(SessionEnvironmentRejection::Terminal);
    }
    if !facts.step.admits_realization_effects() {
        return Err(SessionEnvironmentRejection::ContinuationInProgress);
    }
    stay(facts)
}

/// control.rs `renew_session_realization_after_load`: a terminal Session keeps
/// renewing in every step (terminal cleanup needs its lease); an active one
/// only outside a continuation. The gate precedes the lease check.
const fn renew_realization(facts: SessionEnvironmentFacts) -> Outcome {
    if is_active(facts) && !facts.step.admits_realization_effects() {
        return Err(SessionEnvironmentRejection::ContinuationInProgress);
    }
    if !is_fenced(facts) {
        return Err(SessionEnvironmentRejection::LeaseNotCurrent);
    }
    stay(facts)
}

/// `SessionEnvironmentOperations::admit_environment_effect` over
/// `PersistedSession::authorize_environment_effect` and
/// `SessionEnvironmentState::authorize_effect`.
const fn admit_materialization(
    facts: SessionEnvironmentFacts,
    kind: MaterializationKind,
    already_applied: bool,
) -> Outcome {
    if !is_active(facts) {
        return Err(SessionEnvironmentRejection::Terminal);
    }
    if is_stale(facts) {
        return Err(SessionEnvironmentRejection::LeaseNotCurrent);
    }
    if let Err(rejection) = materialization_admitted(facts.step, kind, already_applied) {
        return Err(rejection);
    }
    // A Rebuild admission waits for source closure and access, which neither a
    // target receipt nor the current lease supplies, even when its effect is
    // already recorded.
    match kind {
        MaterializationKind::Rebuild => Err(SessionEnvironmentRejection::MaterializationPending),
        _ if already_applied => stay(facts),
        MaterializationKind::Create if !is_fenced(facts) => {
            Err(SessionEnvironmentRejection::LeaseNotCurrent)
        }
        _ => instruct(facts.step, SessionEnvironmentInstruction::Materialize(kind)),
    }
}

/// The step × kind table shared by effect admission and receipt application.
/// A Resident replay of its own effect is admitted before the table.
const fn materialization_admitted(
    step: SessionEnvironmentStep,
    kind: MaterializationKind,
    already_applied: bool,
) -> Result<(), SessionEnvironmentRejection> {
    use MaterializationKind as K;
    use SessionEnvironmentStep as S;
    match (step, kind) {
        (S::Suspending(_) | S::Hibernated | S::Restoring, _) => {
            Err(SessionEnvironmentRejection::ContinuationInProgress)
        }
        (S::Resident, _) if already_applied => Ok(()),
        (S::Unmaterialized, K::Create | K::Adopt) => Ok(()),
        (S::Unmaterialized, K::Rebuild | K::ReserveResources) => {
            Err(SessionEnvironmentRejection::RequiresResident)
        }
        (S::Resident, K::Adopt | K::Rebuild | K::ReserveResources) => Ok(()),
        (S::Resident, K::Create) => Err(SessionEnvironmentRejection::InvalidTransition),
    }
}

/// `PersistedSession::try_apply_environment_receipt`: a terminal Session
/// admits a receipt only during the cleanup's Fenced join.
const fn materialized(
    facts: SessionEnvironmentFacts,
    kind: MaterializationKind,
    already_applied: bool,
) -> Outcome {
    if matches!(facts.terminality, SessionTerminality::Terminal) {
        return Err(SessionEnvironmentRejection::Terminal);
    }
    if is_stale(facts) {
        return Err(SessionEnvironmentRejection::LeaseNotCurrent);
    }
    if let Err(rejection) = materialization_admitted(facts.step, kind, already_applied) {
        return Err(rejection);
    }
    enter(SessionEnvironmentStep::Resident)
}

/// `SessionEnvironmentState::deliver_legacy_binding`, the realization's
/// claim-first repair: only an Unmaterialized projection accepts the
/// historical binding. A terminal Session refuses it because a claim is not an
/// in-flight receipt the cleanup join must admit.
const fn legacy_binding_delivered(facts: SessionEnvironmentFacts) -> Outcome {
    if !is_active(facts) {
        return Err(SessionEnvironmentRejection::Terminal);
    }
    match facts.step {
        SessionEnvironmentStep::Unmaterialized => enter(SessionEnvironmentStep::Resident),
        _ => stay(facts),
    }
}

/// continuation.rs `advance_environment_continuation` arm selection, with the
/// frozen-topology guard, the suspend Memory preflight and the
/// `begin_suspend_at` preconditions of the aggregate wrapper.
const fn advance_continuation(facts: SessionEnvironmentFacts) -> Outcome {
    if !is_active(facts) {
        return Err(SessionEnvironmentRejection::Terminal);
    }
    if !facts.continuation_topology_supported {
        return Err(SessionEnvironmentRejection::UnsupportedContinuationTopology);
    }
    if is_stale(facts) {
        return Err(SessionEnvironmentRejection::LeaseNotCurrent);
    }
    match facts.step {
        SessionEnvironmentStep::Resident if facts.suspend_due && facts.execution_idle => {
            if !facts.suspend_memory_admitted {
                Err(SessionEnvironmentRejection::MemoryEvidenceUnproven)
            } else if facts.activities_open {
                Err(SessionEnvironmentRejection::ActivitiesOpen)
            } else if !facts.resident_generated {
                Err(SessionEnvironmentRejection::NotGenerated)
            } else {
                instruct(
                    SessionEnvironmentStep::Suspending(SuspendPhase::Quiescing),
                    SessionEnvironmentInstruction::Quiesce,
                )
            }
        }
        SessionEnvironmentStep::Suspending(phase) => suspend_work(facts, phase),
        SessionEnvironmentStep::Unmaterialized
        | SessionEnvironmentStep::Resident
        | SessionEnvironmentStep::Hibernated
        | SessionEnvironmentStep::Restoring => stay(facts),
    }
}

/// The effect a committed suspend sub-phase demands next.
const fn suspend_work(facts: SessionEnvironmentFacts, phase: SuspendPhase) -> Outcome {
    if !facts.suspend_evidence_consistent {
        return Err(SessionEnvironmentRejection::InconsistentEvidence);
    }
    let step = facts.step;
    match phase {
        SuspendPhase::Quiescing => instruct(step, SessionEnvironmentInstruction::Quiesce),
        SuspendPhase::Uploading if !facts.generation_live => {
            Err(SessionEnvironmentRejection::GenerationExpired)
        }
        SuspendPhase::Uploading => instruct(step, SessionEnvironmentInstruction::Checkpoint),
        SuspendPhase::ReadyToDispose | SuspendPhase::Disposing if !is_fenced(facts) => {
            Err(SessionEnvironmentRejection::LeaseNotCurrent)
        }
        SuspendPhase::ReadyToDispose => {
            instruct(step, SessionEnvironmentInstruction::PrepareSourceRelease)
        }
        SuspendPhase::Disposing => instruct(step, SessionEnvironmentInstruction::DisposeSource),
    }
}

/// continuation.rs `ensure_environment_resident` state dispatch, after the
/// frozen-topology guard.
const fn require_resident(facts: SessionEnvironmentFacts) -> Outcome {
    if !is_active(facts) {
        return Err(SessionEnvironmentRejection::Terminal);
    }
    if !facts.continuation_topology_supported {
        return Err(SessionEnvironmentRejection::UnsupportedContinuationTopology);
    }
    if is_stale(facts) {
        return Err(SessionEnvironmentRejection::LeaseNotCurrent);
    }
    match facts.step {
        SessionEnvironmentStep::Unmaterialized | SessionEnvironmentStep::Resident => stay(facts),
        SessionEnvironmentStep::Suspending(phase) => suspend_work(facts, phase),
        SessionEnvironmentStep::Hibernated if !(facts.checkpoint_live && facts.generation_live) => {
            instruct(
                facts.step,
                SessionEnvironmentInstruction::DeleteExpiredCheckpoint,
            )
        }
        SessionEnvironmentStep::Hibernated if facts.activities_open => {
            Err(SessionEnvironmentRejection::ActivitiesOpen)
        }
        SessionEnvironmentStep::Hibernated => instruct(
            SessionEnvironmentStep::Restoring,
            SessionEnvironmentInstruction::Restore,
        ),
        SessionEnvironmentStep::Restoring => {
            instruct(facts.step, SessionEnvironmentInstruction::Restore)
        }
    }
}

/// `PersistedSession::authorize_terminal_cleanup_effect`: terminal only, under
/// the current lease generation, from every step (Restoring carries its exact
/// restore target; Disposing hands over its durable preparation).
const fn retire_for_cleanup(facts: SessionEnvironmentFacts) -> Outcome {
    if is_active(facts) {
        return Err(SessionEnvironmentRejection::NotTerminal);
    }
    if !is_fenced(facts) {
        return Err(SessionEnvironmentRejection::LeaseNotCurrent);
    }
    instruct(facts.step, SessionEnvironmentInstruction::Retire)
}

const fn continuation_observation_admitted(
    facts: SessionEnvironmentFacts,
) -> Result<SuspendPhase, SessionEnvironmentRejection> {
    if !is_active(facts) {
        return Err(SessionEnvironmentRejection::Terminal);
    }
    match facts.step {
        SessionEnvironmentStep::Suspending(_) if !facts.suspend_evidence_consistent => {
            Err(SessionEnvironmentRejection::InconsistentEvidence)
        }
        SessionEnvironmentStep::Suspending(phase) => Ok(phase),
        _ => Err(SessionEnvironmentRejection::WrongStep),
    }
}

/// `SessionEnvironmentState::record_quiescence`: later sub-phases replay.
const fn quiesced(facts: SessionEnvironmentFacts) -> Outcome {
    match continuation_observation_admitted(facts) {
        Err(rejection) => Err(rejection),
        Ok(SuspendPhase::Quiescing) => {
            enter(SessionEnvironmentStep::Suspending(SuspendPhase::Uploading))
        }
        Ok(_) => stay(facts),
    }
}

/// `SessionEnvironmentState::record_checkpoint`: ReadyToDispose and Disposing
/// replay the same checkpoint; Quiescing has not drained yet.
const fn checkpointed(facts: SessionEnvironmentFacts) -> Outcome {
    match continuation_observation_admitted(facts) {
        Err(rejection) => Err(rejection),
        Ok(SuspendPhase::Quiescing) => Err(SessionEnvironmentRejection::WrongStep),
        Ok(SuspendPhase::Uploading) => enter(SessionEnvironmentStep::Suspending(
            SuspendPhase::ReadyToDispose,
        )),
        Ok(SuspendPhase::ReadyToDispose | SuspendPhase::Disposing) => stay(facts),
    }
}

/// `PersistedSession::record_source_release_prepared`: the lease generation is
/// checked before the phase.
const fn source_release_prepared(facts: SessionEnvironmentFacts) -> Outcome {
    if !is_active(facts) {
        return Err(SessionEnvironmentRejection::Terminal);
    }
    if !is_fenced(facts) {
        return Err(SessionEnvironmentRejection::LeaseNotCurrent);
    }
    match continuation_observation_admitted(facts) {
        Err(rejection) => Err(rejection),
        Ok(SuspendPhase::ReadyToDispose) => {
            enter(SessionEnvironmentStep::Suspending(SuspendPhase::Disposing))
        }
        Ok(SuspendPhase::Disposing) => stay(facts),
        Ok(SuspendPhase::Quiescing | SuspendPhase::Uploading) => {
            Err(SessionEnvironmentRejection::WrongStep)
        }
    }
}

/// `SessionEnvironmentState::complete_suspend`: Hibernated replays.
const fn source_released(facts: SessionEnvironmentFacts) -> Outcome {
    if !is_active(facts) {
        return Err(SessionEnvironmentRejection::Terminal);
    }
    if matches!(facts.step, SessionEnvironmentStep::Hibernated) {
        return stay(facts);
    }
    match continuation_observation_admitted(facts) {
        Err(rejection) => Err(rejection),
        Ok(SuspendPhase::Disposing) => enter(SessionEnvironmentStep::Hibernated),
        Ok(_) => Err(SessionEnvironmentRejection::WrongStep),
    }
}

/// `SessionEnvironmentState::complete_restore`.
const fn restored(facts: SessionEnvironmentFacts) -> Outcome {
    if !is_active(facts) {
        return Err(SessionEnvironmentRejection::Terminal);
    }
    match facts.step {
        SessionEnvironmentStep::Restoring => enter(SessionEnvironmentStep::Resident),
        _ => Err(SessionEnvironmentRejection::WrongStep),
    }
}

/// `SessionEnvironmentOperations::discard_expired_checkpoint`, reached only
/// through `ensure_environment_resident` after the idempotent delete.
const fn expired_checkpoint_deleted(facts: SessionEnvironmentFacts) -> Outcome {
    if !is_active(facts) {
        return Err(SessionEnvironmentRejection::Terminal);
    }
    match facts.step {
        SessionEnvironmentStep::Hibernated if facts.checkpoint_live && facts.generation_live => {
            Err(SessionEnvironmentRejection::CheckpointLive)
        }
        SessionEnvironmentStep::Hibernated => enter(SessionEnvironmentStep::Unmaterialized),
        _ => Err(SessionEnvironmentRejection::WrongStep),
    }
}

/// `PersistedSession::record_terminal_cleanup_disposal`: the completed replay
/// needs no lease; the first disposal retires the Environment from any step.
const fn cleaned_up(facts: SessionEnvironmentFacts, already_recorded: bool) -> Outcome {
    if is_active(facts) {
        return Err(SessionEnvironmentRejection::NotTerminal);
    }
    if already_recorded {
        return stay(facts);
    }
    if !is_fenced(facts) {
        return Err(SessionEnvironmentRejection::LeaseNotCurrent);
    }
    enter(SessionEnvironmentStep::Unmaterialized)
}

impl SessionEnvironmentFacts {
    /// Fold the aggregate, the caller's asserted lease and `now` into the
    /// facts the transition function reads. `asserted_lease` is the lease an
    /// event carries; `None` means the event acts under the root's own lease.
    #[must_use]
    pub fn project(
        session: &crate::PersistedSession,
        now_unix_ms: u64,
        asserted_lease: Option<&crate::SessionRealizationLease>,
    ) -> Self {
        let terminality = if !session.is_terminal() {
            SessionTerminality::Active
        } else if session.terminal_cleanup.is_fenced() {
            SessionTerminality::CleanupJoining
        } else {
            SessionTerminality::Terminal
        };
        let lease = match (session.realization.as_ref(), asserted_lease) {
            (Some(_), None) => RealizationLeaseStanding::Fenced,
            (None, None) => RealizationLeaseStanding::Unfenced,
            (Some(current), Some(asserted))
                if crate::realization_lease_authorizes(current, asserted, now_unix_ms) =>
            {
                RealizationLeaseStanding::Fenced
            }
            _ => RealizationLeaseStanding::Stale,
        };
        let (resident_generated, suspend_due) = match &session.environment {
            SessionEnvironmentState::Resident {
                effect_id,
                generation,
                idle_since_unix_ms,
                ..
            } => {
                let policy = session
                    .frozen_baseline()
                    .map(|baseline| &baseline.environment.idle_retention);
                let due = match (generation, idle_since_unix_ms, policy) {
                    (Some(_), Some(idle_since), Some(policy)) => {
                        policy.mode == crate::EnvironmentIdleRetentionMode::CheckpointAndRelease
                            && now_unix_ms
                                >= idle_since.saturating_add(
                                    policy.checkpoint_after_secs.saturating_mul(1_000),
                                )
                    }
                    _ => false,
                };
                (effect_id.is_some() && generation.is_some(), due)
            }
            _ => (false, false),
        };
        Self {
            step: session.environment.step(),
            terminality,
            execution_idle: session.execution == crate::SessionExecutionState::Idle,
            activities_open: session.has_active_activities(),
            lease,
            resident_generated,
            suspend_due,
            suspend_evidence_consistent: session.environment.suspend_evidence_is_consistent(),
            generation_live: !session
                .environment
                .generation()
                .is_some_and(|generation| generation.expired_at(now_unix_ms)),
            checkpoint_live: !session
                .environment
                .checkpoint()
                .is_some_and(|checkpoint| checkpoint.expired_at(now_unix_ms)),
            continuation_topology_supported: session.frozen_baseline().is_none_or(|baseline| {
                crate::SessionPlacementClass::new(
                    baseline.runtime_placement,
                    baseline.environment.self_hosted,
                )
                .supports_idle_retention(&baseline.environment.idle_retention)
            }),
            suspend_memory_admitted: matches!(
                session.environment,
                SessionEnvironmentState::Resident { .. }
            ) && suspend_memory_preflight(session).is_ok(),
        }
    }

    /// Project the facts for a continuation command (advance or require
    /// resident). The asserted lease is the one whose fence the step's next
    /// effect presents: a committed quiesce, checkpoint or restore operation
    /// presents the lease it captured, so a root that no longer authorizes it
    /// refuses the effect before provider I/O. Every other step acts under the
    /// root's current lease: source release and disposal lower it (a
    /// higher-epoch successor may finish them), and Resident and Hibernated
    /// commit a new operation under it.
    #[must_use]
    pub fn project_continuation(session: &crate::PersistedSession, now_unix_ms: u64) -> Self {
        let asserted = match &session.environment {
            SessionEnvironmentState::Suspending {
                operation,
                suspend_phase: SuspendPhase::Quiescing | SuspendPhase::Uploading,
                ..
            }
            | SessionEnvironmentState::Restoring { operation, .. } => {
                operation.realization.as_ref()
            }
            _ => None,
        };
        Self::project(session, now_unix_ms, asserted)
    }

    /// The lifecycle supervisor's continuation hint, read from the
    /// aggregate's own decision: whether `AdvanceContinuation`, projected at
    /// `now`, instructs an effect. A refusal is not work; the root re-enters
    /// the hint in the cycle its facts admit the effect.
    #[must_use]
    pub fn continuation_due(session: &crate::PersistedSession, now_unix_ms: u64) -> bool {
        matches!(
            transition_environment(
                Self::project_continuation(session, now_unix_ms),
                SessionEnvironmentEvent::AdvanceContinuation,
            ),
            Ok(decision) if decision.instruction().is_some()
        )
    }

    /// Project the facts of a terminal cleanup event (`RetireForCleanup`,
    /// `CleanedUp`). Terminal cleanup acts under the root's own lease
    /// generation, so the event asserts no lease, and both rows read only the
    /// terminality and the lease standing: no time-dependent fact can change
    /// their decision (table T11), so the projection needs no clock.
    pub(crate) fn project_terminal_cleanup(session: &crate::PersistedSession) -> Self {
        Self::project(session, 0, None)
    }

    /// Project the facts of a realization directive's frozen Environment
    /// state, for the one event a directive's projection receives
    /// (`LegacyBindingDelivered`). Session-application issues a directive only
    /// under `BeginRealization`'s admission, which refuses a terminal Session,
    /// and the directive carries the lease it was issued under: the projection
    /// stands Active and Fenced. It is a copy, not the live aggregate, so it
    /// asserts no execution, activity, time or placement fact and admits no
    /// continuation.
    pub(crate) fn project_realization_directive(environment: &SessionEnvironmentState) -> Self {
        let resident_generated = matches!(
            environment,
            SessionEnvironmentState::Resident {
                effect_id: Some(_),
                generation: Some(_),
                ..
            }
        );
        Self {
            step: environment.step(),
            terminality: SessionTerminality::Active,
            execution_idle: false,
            activities_open: false,
            lease: RealizationLeaseStanding::Fenced,
            resident_generated,
            suspend_due: false,
            suspend_evidence_consistent: environment.suspend_evidence_is_consistent(),
            generation_live: false,
            checkpoint_live: false,
            continuation_topology_supported: false,
            suspend_memory_admitted: false,
        }
    }
}

/// The suspend Memory preflight over the aggregate's current source binding.
/// A pending Memory generation has no single physical input/evidence join and
/// therefore remains resident; active Memory reuses the neutral None/Some
/// validator that source disposal invokes on historical rows.
fn suspend_memory_preflight(
    session: &crate::PersistedSession,
) -> Result<(), crate::SessionMemoryReconciliationError> {
    let is_memory = |input: &crate::ResolvedInput| {
        matches!(
            &input.source,
            crate::ResolvedInputSource::MemoryStore { .. }
        )
    };
    if session
        .resources
        .pending
        .as_ref()
        .is_some_and(|pending| pending.inputs().iter().any(is_memory))
    {
        return Err(crate::SessionMemoryReconciliationError::ResourceMismatch);
    }
    let active_inputs = session.resources.active.inputs();
    if !active_inputs.iter().any(is_memory) {
        return Ok(());
    }
    let binding = session
        .environment
        .binding()
        .ok_or(crate::SessionMemoryReconciliationError::EnvironmentMismatch)?;
    let handle = serde_json::from_str::<awaken_provisioning_contract::SandboxHandle>(binding)
        .map_err(|error| {
            crate::SessionMemoryReconciliationError::InvalidIntent(error.to_string())
        })?;
    let materializations = handle.memory_materializations().map_err(|error| {
        crate::SessionMemoryReconciliationError::InvalidIntent(error.to_string())
    })?;
    crate::validate_continuation_memory_materializations(active_inputs, materializations)?;
    Ok(())
}

/// The safety laws the decision table and the Kani harnesses both check, so
/// the exhaustive test sweep and the proof state one predicate each.
#[cfg(any(test, kani))]
mod laws {
    use super::*;
    use SessionEnvironmentEvent as E;
    use SessionEnvironmentStep as S;

    pub(super) const STEPS: [SessionEnvironmentStep; 8] = [
        S::Unmaterialized,
        S::Resident,
        S::Suspending(SuspendPhase::Quiescing),
        S::Suspending(SuspendPhase::Uploading),
        S::Suspending(SuspendPhase::ReadyToDispose),
        S::Suspending(SuspendPhase::Disposing),
        S::Hibernated,
        S::Restoring,
    ];

    const KINDS: [MaterializationKind; 4] = [
        MaterializationKind::Create,
        MaterializationKind::Adopt,
        MaterializationKind::Rebuild,
        MaterializationKind::ReserveResources,
    ];

    /// Every closed event value, in one fixed order.
    pub(super) const fn event(index: usize) -> SessionEnvironmentEvent {
        match index {
            0 => E::BeginRealization,
            1 => E::RenewRealization,
            2..=9 => E::AdmitMaterialization {
                kind: KINDS[(index - 2) % 4],
                already_applied: index >= 6,
            },
            10 => E::AdvanceContinuation,
            11 => E::RequireResident,
            12 => E::RetireForCleanup,
            13..=20 => E::Materialized {
                kind: KINDS[(index - 13) % 4],
                already_applied: index >= 17,
            },
            21 => E::LegacyBindingDelivered,
            22 => E::Quiesced,
            23 => E::Checkpointed,
            24 => E::SourceReleasePrepared,
            25 => E::SourceReleased,
            26 => E::Restored,
            27 => E::ExpiredCheckpointDeleted,
            28 => E::CleanedUp {
                already_recorded: false,
            },
            29 => E::CleanedUp {
                already_recorded: true,
            },
            _ => E::EffectFailed,
        }
    }

    pub(super) const EVENT_COUNT: usize = 31;

    /// L1: a terminal Session never re-enters a live Environment step. Outside
    /// the cleanup's Fenced join it only retires to Unmaterialized; inside the
    /// join an in-flight materialization receipt may still land. Continuation
    /// and materialization effects are never instructed for it.
    pub(super) fn terminal_never_reenters(
        facts: SessionEnvironmentFacts,
        event: SessionEnvironmentEvent,
        outcome: Outcome,
    ) -> bool {
        let Ok(decision) = outcome else {
            return true;
        };
        if matches!(facts.terminality, SessionTerminality::Active) {
            return true;
        }
        let moved_legally = decision.next == facts.step
            || (decision.next == S::Unmaterialized && matches!(event, E::CleanedUp { .. }))
            || (decision.next == S::Resident
                && matches!(facts.terminality, SessionTerminality::CleanupJoining)
                && matches!(event, E::Materialized { .. }));
        let instruction_legal = matches!(
            decision.instruction,
            None | Some(SessionEnvironmentInstruction::Retire)
        );
        moved_legally && instruction_legal
    }

    /// L2: every step change is one declared edge; in particular Resident is
    /// entered only from Unmaterialized by materialization (or the legacy
    /// binding) or from Restoring by its receipt, never from Suspending or
    /// Hibernated without a restore.
    pub(super) fn steps_follow_declared_edges(
        facts: SessionEnvironmentFacts,
        event: SessionEnvironmentEvent,
        outcome: Outcome,
    ) -> bool {
        let Ok(decision) = outcome else {
            return true;
        };
        let (from, to) = (facts.step, decision.next);
        if from == to {
            return true;
        }
        let edge = match (from, to) {
            (S::Unmaterialized, S::Resident) => {
                matches!(event, E::Materialized { .. } | E::LegacyBindingDelivered)
            }
            (S::Resident, S::Suspending(SuspendPhase::Quiescing)) => {
                matches!(event, E::AdvanceContinuation)
            }
            (S::Suspending(SuspendPhase::Quiescing), S::Suspending(SuspendPhase::Uploading)) => {
                matches!(event, E::Quiesced)
            }
            (
                S::Suspending(SuspendPhase::Uploading),
                S::Suspending(SuspendPhase::ReadyToDispose),
            ) => matches!(event, E::Checkpointed),
            (
                S::Suspending(SuspendPhase::ReadyToDispose),
                S::Suspending(SuspendPhase::Disposing),
            ) => matches!(event, E::SourceReleasePrepared),
            (S::Suspending(SuspendPhase::Disposing), S::Hibernated) => {
                matches!(event, E::SourceReleased)
            }
            (S::Hibernated, S::Restoring) => matches!(event, E::RequireResident),
            (S::Restoring, S::Resident) => matches!(event, E::Restored),
            (S::Hibernated, S::Unmaterialized) => {
                matches!(event, E::ExpiredCheckpointDeleted | E::CleanedUp { .. })
            }
            (_, S::Unmaterialized) => matches!(event, E::CleanedUp { .. }),
            _ => false,
        };
        let resident_entry = to != S::Resident
            || matches!(
                (from, event),
                (
                    S::Unmaterialized,
                    E::Materialized { .. } | E::LegacyBindingDelivered
                ) | (S::Restoring, E::Restored)
            );
        edge && resident_entry
    }

    /// L3: no effect is instructed under a stale lease, and every instruction
    /// that creates, destroys or retires physical state, as well as every
    /// observation that advances one, requires the current lease.
    pub(super) fn effects_require_current_lease(
        facts: SessionEnvironmentFacts,
        event: SessionEnvironmentEvent,
        outcome: Outcome,
    ) -> bool {
        let Ok(decision) = outcome else {
            return true;
        };
        let fenced = matches!(facts.lease, RealizationLeaseStanding::Fenced);
        let instruction_ok = match decision.instruction {
            None => true,
            Some(instruction) => {
                !matches!(facts.lease, RealizationLeaseStanding::Stale)
                    && (!instruction.requires_fenced_lease() || fenced)
            }
        };
        let destructive_observation = decision.next != facts.step
            && matches!(event, E::SourceReleasePrepared | E::CleanedUp { .. });
        instruction_ok && (!destructive_observation || fenced)
    }
}

#[cfg(test)]
mod tests;

#[cfg(kani)]
mod proofs;
