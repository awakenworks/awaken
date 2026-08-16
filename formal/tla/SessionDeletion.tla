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
          deletedFactCount, revision, processUp

vars == <<disposition, executionState, cleanupPhase, visible,
          cleanupEffectId, lastAttemptEffectId, cleanupAttempts,
          cleanupFailures, workRetired, resourceReleaseRequested,
          deletedFactCount, revision, processUp>>

NoEffect == "none"
DeleteEffect == "session-cleanup:session-1"

Init ==
    /\ disposition = "active"
    /\ executionState = "running"
    /\ cleanupPhase = "not_requested"
    /\ visible = TRUE
    /\ cleanupEffectId = NoEffect
    /\ lastAttemptEffectId = NoEffect
    /\ cleanupAttempts = 0
    /\ cleanupFailures = 0
    /\ workRetired = FALSE
    /\ resourceReleaseRequested = FALSE
    /\ deletedFactCount = 0
    /\ revision = 0
    /\ processUp = TRUE

\* The first delete request atomically records the lifecycle fact, hides and
\* terminalizes the Session, and installs the stable cleanup effect identity.
CommitDeleteIntent ==
    /\ processUp
    /\ disposition = "active"
    /\ revision < MaxRevision
    /\ disposition' = "deleting"
    /\ executionState' = "terminated"
    /\ cleanupPhase' = "fenced"
    /\ visible' = FALSE
    /\ cleanupEffectId' = DeleteEffect
    /\ deletedFactCount' = deletedFactCount + 1
    /\ revision' = revision + 1
    /\ UNCHANGED <<lastAttemptEffectId, cleanupAttempts,
                    cleanupFailures, workRetired, resourceReleaseRequested, processUp>>

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
    /\ cleanupPhase' = "requested"
    /\ resourceReleaseRequested' = TRUE
    /\ revision' = revision + 1
    /\ UNCHANGED <<disposition, executionState, visible, cleanupEffectId,
                    lastAttemptEffectId, cleanupAttempts, cleanupFailures,
                    workRetired, deletedFactCount, processUp>>

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
                    deletedFactCount, revision, processUp>>

\* A failed external attempt records no completion authority. Failures are
\* bounded only so TLC can also check eventual recovery under fairness.
CleanupFails ==
    /\ processUp
    /\ disposition = "deleting"
    /\ cleanupPhase = "requested"
    /\ cleanupFailures < MaxCleanupFailures
    /\ cleanupFailures' = cleanupFailures + 1
    /\ cleanupAttempts' = cleanupAttempts + 1
    /\ lastAttemptEffectId' = cleanupEffectId
    /\ UNCHANGED <<disposition, executionState, cleanupPhase, visible,
                    cleanupEffectId, workRetired, resourceReleaseRequested,
                    deletedFactCount, revision, processUp>>

\* Only an exact successful cleanup attempt advances durable completion.
CleanupSucceeds ==
    /\ processUp
    /\ disposition = "deleting"
    /\ cleanupPhase = "requested"
    /\ workRetired
    /\ resourceReleaseRequested
    /\ revision < MaxRevision
    /\ cleanupPhase' = "completed"
    /\ cleanupAttempts' = cleanupAttempts + 1
    /\ lastAttemptEffectId' = cleanupEffectId
    /\ revision' = revision + 1
    /\ UNCHANGED <<disposition, executionState, visible, cleanupEffectId,
                    cleanupFailures, workRetired, resourceReleaseRequested,
                    deletedFactCount, processUp>>

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
                    workRetired, resourceReleaseRequested, deletedFactCount, processUp>>

Crash ==
    /\ processUp
    /\ disposition # "tombstoned"
    /\ processUp' = FALSE
    /\ UNCHANGED <<disposition, executionState, cleanupPhase, visible,
                    cleanupEffectId, lastAttemptEffectId, cleanupAttempts,
                    cleanupFailures, workRetired, resourceReleaseRequested,
                    deletedFactCount, revision>>

Restart ==
    /\ ~processUp
    /\ processUp' = TRUE
    /\ UNCHANGED <<disposition, executionState, cleanupPhase, visible,
                    cleanupEffectId, lastAttemptEffectId, cleanupAttempts,
                    cleanupFailures, workRetired, resourceReleaseRequested,
                    deletedFactCount, revision>>

Next ==
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
    /\ SF_vars(RetireWork)
    /\ SF_vars(FreezeCleanupTargets)
    /\ SF_vars(CleanupSucceeds)
    /\ SF_vars(CommitTombstone)

TypeOK ==
    /\ disposition \in {"active", "deleting", "tombstoned"}
    /\ executionState \in {"running", "terminated"}
    /\ cleanupPhase \in {"not_requested", "fenced", "requested", "completed"}
    /\ visible \in BOOLEAN
    /\ cleanupEffectId \in {NoEffect, DeleteEffect}
    /\ lastAttemptEffectId \in {NoEffect, DeleteEffect}
    /\ cleanupAttempts \in 0..(MaxCleanupFailures + 1)
    /\ cleanupFailures \in 0..MaxCleanupFailures
    /\ workRetired \in BOOLEAN
    /\ resourceReleaseRequested \in BOOLEAN
    /\ deletedFactCount \in 0..1
    /\ revision \in 0..MaxRevision
    /\ processUp \in BOOLEAN

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

DeleteSafety ==
    /\ TypeOK
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
