-------------------------- MODULE SessionDeletion --------------------------
EXTENDS Naturals, TLC

CONSTANTS MaxCleanupFailures, MaxRevision
ASSUME MaxCleanupFailures \in Nat
       /\ MaxRevision \in Nat
       /\ MaxRevision >= 4

\* One bounded Session deletion saga. The durable delete fence is committed
\* before cleanup begins; external cleanup may fail, the process may restart,
\* and every retry must reuse the operation's original effect identity.
VARIABLES disposition, executionState, cleanupPhase, visible,
          cleanupEffectId, lastAttemptEffectId, cleanupAttempts,
          cleanupFailures, workRetired, resourceReleaseRequested,
          deletedFactCount, revision, processUp,
          physicalActive, returnObserved, reconstructed, prepared, removedTargets

vars == <<disposition, executionState, cleanupPhase, visible,
          cleanupEffectId, lastAttemptEffectId, cleanupAttempts,
          cleanupFailures, workRetired, resourceReleaseRequested,
          deletedFactCount, revision, processUp,
          physicalActive, returnObserved, reconstructed, prepared, removedTargets>>

NoEffect == "none"
DeleteEffect == "session-cleanup:session-1"
Participants == {"Memory", "Secret", "Staging"}
PhysicalTargets == {"Container", "HostStaging"}

\* R7/R8 evidence is abstract. Rust/provider tests own exact mount, Memory
\* base and Secret broker validation. This existing composition derives the
\* generic kernel's readiness without duplicating its cleanup state machine.

Cleanup == INSTANCE SessionCleanupKernel WITH
    NoEffect <- NoEffect,
    CleanupEffect <- DeleteEffect,
    MaxCleanupFailures <- MaxCleanupFailures,
    kCleanupPhase <- cleanupPhase,
    kCleanupEffectId <- cleanupEffectId,
    kLastAttemptEffectId <- lastAttemptEffectId,
    kCleanupAttempts <- cleanupAttempts,
    kCleanupFailures <- cleanupFailures,
    kResourceReleaseRequested <- resourceReleaseRequested

Init ==
    /\ disposition = "active"
    /\ executionState = "running"
    /\ Cleanup!Init
    /\ visible = TRUE
    /\ workRetired = FALSE
    /\ deletedFactCount = 0
    /\ revision = 0
    /\ processUp = TRUE
    /\ physicalActive = TRUE
    /\ returnObserved = FALSE
    /\ reconstructed = {}
    /\ prepared = {}
    /\ removedTargets = {}

\* The first delete request atomically records the lifecycle fact, hides and
\* terminalizes the Session, and installs the stable cleanup effect identity.
CommitDeleteIntent ==
    /\ processUp
    /\ disposition = "active"
    /\ revision < MaxRevision
    /\ disposition' = "deleting"
    /\ executionState' = "terminated"
    /\ Cleanup!Fence
    /\ visible' = FALSE
    /\ deletedFactCount' = deletedFactCount + 1
    /\ revision' = revision + 1
    /\ UNCHANGED <<workRetired, processUp,
                    physicalActive, returnObserved, reconstructed, prepared, removedTargets>>

\* Reissuing DELETE after the fence is a semantic replay: it cannot publish
\* another fact, replace the effect identity, or reopen the Session.
ReplayDelete ==
    /\ disposition \in {"deleting", "tombstoned"}
    /\ UNCHANGED vars

\* The cleanup target set is frozen durably before any substrate effect.
FreezeCleanupTargets ==
    /\ processUp
    /\ disposition = "deleting"
    /\ cleanupPhase = "fenced"
    /\ revision < MaxRevision
    /\ Cleanup!Freeze
    /\ revision' = revision + 1
    /\ UNCHANGED <<disposition, executionState, visible,
                    workRetired, deletedFactCount, processUp,
                    physicalActive, returnObserved, reconstructed, prepared, removedTargets>>

\* Queue retirement is an idempotent recovery effect. It may run immediately
\* after the delete fence or after restart, but verified cleanup cannot complete
\* until this independent external authority has settled.
RetireWork ==
    /\ processUp
    /\ disposition = "deleting"
    /\ cleanupPhase \in {"fenced", "requested"}
    /\ ~workRetired
    /\ workRetired' = TRUE
    /\ UNCHANGED <<disposition, executionState, cleanupPhase, visible,
                    cleanupEffectId, lastAttemptEffectId, cleanupAttempts,
                    cleanupFailures, resourceReleaseRequested,
                    deletedFactCount, revision, processUp,
                    physicalActive, returnObserved, reconstructed, prepared, removedTargets>>

\* A failed external attempt records no completion authority. Failures are
\* bounded only so TLC can also check eventual recovery under fairness.
CleanupFails ==
    /\ processUp
    /\ disposition = "deleting"
    /\ cleanupPhase = "requested"
    /\ Cleanup!Fail
    /\ UNCHANGED <<disposition, executionState, visible,
                    workRetired,
                    deletedFactCount, revision, processUp,
                    physicalActive, returnObserved, reconstructed, prepared, removedTargets>>

\* Only an exact successful cleanup attempt advances durable completion.
CleanupSucceeds ==
    /\ processUp
    /\ disposition = "deleting"
    /\ cleanupPhase = "requested"
    /\ workRetired
    /\ resourceReleaseRequested
    /\ revision < MaxRevision
    /\ Cleanup!Succeed(workRetired /\ resourceReleaseRequested
                         /\ ~physicalActive /\ prepared = Participants
                         /\ removedTargets = PhysicalTargets)
    /\ revision' = revision + 1
    /\ UNCHANGED <<disposition, executionState, visible,
                    workRetired,
                    deletedFactCount, processUp,
                    physicalActive, returnObserved, reconstructed, prepared, removedTargets>>

\* Physical deletion is a separate durable CAS and is forbidden until the
\* verified cleanup completion is committed.
CommitTombstone ==
    /\ processUp
    /\ disposition = "deleting"
    /\ cleanupPhase = "completed"
    /\ revision < MaxRevision
    /\ disposition' = "tombstoned"
    /\ revision' = revision + 1
    /\ UNCHANGED <<executionState, cleanupPhase, visible, cleanupEffectId,
                    lastAttemptEffectId, cleanupAttempts, cleanupFailures,
                    workRetired, resourceReleaseRequested, deletedFactCount, processUp,
                    physicalActive, returnObserved, reconstructed, prepared, removedTargets>>

Crash ==
    /\ processUp
    /\ disposition # "tombstoned"
    /\ processUp' = FALSE
    /\ reconstructed' = {}
    /\ UNCHANGED <<disposition, executionState, cleanupPhase, visible,
                    cleanupEffectId, lastAttemptEffectId, cleanupAttempts,
                    cleanupFailures, workRetired, resourceReleaseRequested,
                    deletedFactCount, revision,
                    physicalActive, returnObserved, prepared, removedTargets>>

Restart ==
    /\ ~processUp
    /\ processUp' = TRUE
    /\ UNCHANGED <<disposition, executionState, cleanupPhase, visible,
                    cleanupEffectId, lastAttemptEffectId, cleanupAttempts,
                    cleanupFailures, workRetired, resourceReleaseRequested,
                    deletedFactCount, revision,
                    physicalActive, returnObserved, reconstructed, prepared, removedTargets>>

\* R2/R4: only an exact observed attempt return supplies physical evidence.
\* Crash, delete intent and logical termination cannot manufacture this fact.
ObserveAttemptReturn ==
    /\ processUp /\ physicalActive
    /\ physicalActive' = FALSE
    /\ returnObserved' = TRUE
    /\ UNCHANGED <<disposition, executionState, cleanupPhase, visible, cleanupEffectId, lastAttemptEffectId, cleanupAttempts, cleanupFailures, workRetired, resourceReleaseRequested, deletedFactCount, revision, processUp, reconstructed, prepared, removedTargets>>

RecoverParticipant(participant) ==
    /\ processUp /\ cleanupPhase = "requested"
    /\ participant \in Participants \ reconstructed
    /\ reconstructed' = reconstructed \cup {participant}
    /\ UNCHANGED <<disposition, executionState, cleanupPhase, visible, cleanupEffectId, lastAttemptEffectId, cleanupAttempts, cleanupFailures, workRetired, resourceReleaseRequested, deletedFactCount, revision, processUp, physicalActive, returnObserved, prepared, removedTargets>>

PrepareParticipant(participant, effect) ==
    /\ processUp /\ cleanupPhase = "requested"
    /\ ~physicalActive /\ effect = cleanupEffectId
    /\ participant \in reconstructed \ prepared
    /\ prepared' = prepared \cup {participant}
    /\ UNCHANGED <<disposition, executionState, cleanupPhase, visible, cleanupEffectId, lastAttemptEffectId, cleanupAttempts, cleanupFailures, workRetired, resourceReleaseRequested, deletedFactCount, revision, processUp, physicalActive, returnObserved, reconstructed, removedTargets>>

RemoveTarget(target, effect) ==
    /\ processUp /\ cleanupPhase = "requested"
    /\ ~physicalActive /\ prepared = Participants
    /\ effect = cleanupEffectId
    /\ target \in PhysicalTargets \ removedTargets
    /\ removedTargets' = removedTargets \cup {target}
    /\ UNCHANGED <<disposition, executionState, cleanupPhase, visible, cleanupEffectId, lastAttemptEffectId, cleanupAttempts, cleanupFailures, workRetired, resourceReleaseRequested, deletedFactCount, revision, processUp, physicalActive, returnObserved, reconstructed, prepared>>

Next ==
    \/ ObserveAttemptReturn
    \/ \E p \in Participants: RecoverParticipant(p)
    \/ \E p \in Participants, effect \in {DeleteEffect, "foreign"}: PrepareParticipant(p, effect)
    \/ \E t \in PhysicalTargets, effect \in {DeleteEffect, "foreign"}: RemoveTarget(t, effect)
    \/ CommitDeleteIntent
    \/ ReplayDelete
    \/ RetireWork
    \/ FreezeCleanupTargets
    \/ CleanupFails
    \/ CleanupSucceeds
    \/ CommitTombstone
    \/ Crash
    \/ Restart

Spec == Init /\ [][Next]_vars

\* Recovery fairness is an explicit environmental assumption: a crashed
\* coordinator restarts and continuously/repeatedly enabled durable progress
\* eventually runs. Safety invariants do not depend on these assumptions.
FairSpec ==
    /\ Spec
    /\ WF_vars(Restart)
    \* Conditional liveness requires actual return and eventual dependency
    \* recovery. Spec still permits permanent unknown evidence safely.
    /\ SF_vars(ObserveAttemptReturn)
    /\ \A p \in Participants: SF_vars(RecoverParticipant(p))
    /\ \A p \in Participants: SF_vars(PrepareParticipant(p, DeleteEffect))
    /\ \A t \in PhysicalTargets: SF_vars(RemoveTarget(t, DeleteEffect))
    /\ SF_vars(RetireWork)
    /\ SF_vars(FreezeCleanupTargets)
    /\ SF_vars(CleanupSucceeds)
    /\ SF_vars(CommitTombstone)

TypeOK ==
    /\ disposition \in {"active", "deleting", "tombstoned"}
    /\ executionState \in {"running", "terminated"}
    /\ Cleanup!TypeOK
    /\ visible \in BOOLEAN
    /\ workRetired \in BOOLEAN
    /\ deletedFactCount \in 0..1
    /\ revision \in 0..MaxRevision
    /\ processUp \in BOOLEAN
    /\ physicalActive \in BOOLEAN /\ returnObserved \in BOOLEAN
    /\ reconstructed \subseteq Participants /\ prepared \subseteq Participants
    /\ removedTargets \subseteq PhysicalTargets

HiddenDeleteStateIsTerminal ==
    disposition \in {"deleting", "tombstoned"}
        => (~visible /\ executionState = "terminated")

VisibilityMatchesDurableDisposition ==
    visible <=> disposition = "active"

TombstoneRequiresCompletedCleanup ==
    disposition = "tombstoned" => (cleanupPhase = "completed" /\ workRetired)

CompletedCleanupRequiresRetiredWork ==
    cleanupPhase = "completed" => (workRetired /\ resourceReleaseRequested)

FrozenTargetsOwnResourceRelease ==
    cleanupPhase \in {"requested", "completed"} => resourceReleaseRequested

CleanupUsesStableEffectId ==
    /\ cleanupPhase # "not_requested" => cleanupEffectId = DeleteEffect
    /\ lastAttemptEffectId # NoEffect => lastAttemptEffectId = cleanupEffectId

DeletedFactIsCommittedAtMostOnce ==
    /\ deletedFactCount <= 1
    /\ disposition \in {"deleting", "tombstoned"} => deletedFactCount = 1

PhysicalEvidenceRequired ==
    /\ physicalActive = ~returnObserved
    /\ prepared # {} => ~physicalActive
    /\ removedTargets # {} => (~physicalActive /\ prepared = Participants)
    /\ cleanupPhase = "completed" =>
          (~physicalActive /\ prepared = Participants /\ removedTargets = PhysicalTargets)

DeleteSafety ==
    /\ PhysicalEvidenceRequired
    /\ TypeOK
    /\ Cleanup!Safety
    /\ HiddenDeleteStateIsTerminal
    /\ VisibilityMatchesDurableDisposition
    /\ TombstoneRequiresCompletedCleanup
    /\ CompletedCleanupRequiresRetiredWork
    /\ FrozenTargetsOwnResourceRelease
    /\ CleanupUsesStableEffectId
    /\ DeletedFactIsCommittedAtMostOnce

DeleteEventuallyTombstones ==
    disposition = "deleting" ~> disposition = "tombstoned"

FailureEventuallyRecovers ==
    (cleanupFailures > 0 /\ disposition = "deleting")
        ~> disposition = "tombstoned"
=============================================================================
