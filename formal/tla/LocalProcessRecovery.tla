---------------------- MODULE LocalProcessRecovery ----------------------
EXTENDS LocalProcessLifecycle

\* Compose, do not duplicate, the existing GO/current-authority/kernel model.
\* One execution, one Ready realization and two competing recovery entrances.
\* Session authorization, marker atomic replacement and truthful OS evidence
\* are assumptions. This is not a Rust refinement or kernel correctness proof.
VARIABLES markerPhase, markerRecorded, markerLock, observerKnown,
          recoveryObservation, rootPresent, resourceEffect, deletion,
          handQuiescent, sessionReceipt
VARIABLES cleanupCaller, cleanupStage, cleanupFenceLive, controlQuiescent,
          mountsReleased, cleanupQueued
cleanupVars == <<cleanupCaller, cleanupStage, cleanupFenceLive,
                controlQuiescent, mountsReleased, cleanupQueued>>
markerVars == <<markerPhase, markerRecorded, markerLock, observerKnown,
               recoveryObservation, rootPresent, resourceEffect, deletion,
               handQuiescent, sessionReceipt>>
allVars == <<allBoundaryVars, renewalVars, markerVars, cleanupVars>>

RecoveryInit ==
  /\ Init
  /\ markerPhase = "ready" /\ markerRecorded = FALSE /\ markerLock = "free"
  /\ observerKnown = TRUE /\ recoveryObservation = "none"
  /\ rootPresent = TRUE /\ resourceEffect = FALSE /\ deletion = FALSE
  /\ handQuiescent = FALSE /\ sessionReceipt = FALSE
  /\ cleanupCaller = NoLock /\ cleanupStage = "idle"
  /\ cleanupFenceLive = TRUE /\ controlQuiescent = FALSE
  /\ mountsReleased = FALSE /\ cleanupQueued = {}

RecoveryBoundary ==
  /\ BoundaryNext /\ UNCHANGED renewalVars
  /\ ((~scopePermit /\ scopePermit') =>
         markerLock = "free" /\ markerPhase = "ready" /\ rootPresent)
  /\ ((~goIssued /\ goIssued') => markerRecorded /\ markerLock = "launch")
  /\ markerLock' = IF ~scopePermit /\ scopePermit' THEN "launch"
                    ELSE IF phase # "crashed" /\ phase' = "crashed"
                         THEN "free" ELSE markerLock
  /\ cleanupCaller' = IF phase # "crashed" /\ phase' = "crashed"
                       THEN NoLock ELSE cleanupCaller
  /\ cleanupStage' = IF phase # "crashed" /\ phase' = "crashed"
                      THEN "idle" ELSE cleanupStage
  /\ cleanupQueued' = IF phase # "crashed" /\ phase' = "crashed"
                       THEN {} ELSE cleanupQueued
  /\ UNCHANGED <<cleanupFenceLive, controlQuiescent, mountsReleased>>
  /\ UNCHANGED <<markerPhase, markerRecorded, observerKnown,
                 recoveryObservation, rootPresent, resourceEffect, deletion,
                 handQuiescent, sessionReceipt>>

PersistBeforeGo ==
  /\ UNCHANGED cleanupVars
  /\ markerLock = "launch" /\ authorityHeld /\ launchStage = "acknowledged"
  /\ ~markerRecorded /\ markerRecorded' = TRUE
  /\ UNCHANGED <<allBoundaryVars, renewalVars, markerPhase, markerLock,
                 observerKnown, recoveryObservation, rootPresent, resourceEffect, deletion,
                 handQuiescent, sessionReceipt>>

ReleaseLaunch ==
  /\ UNCHANGED cleanupVars
  /\ markerLock = "launch" /\ ~scopePermit
  /\ markerLock' = "free"
  /\ UNCHANGED <<allBoundaryVars, renewalVars, markerPhase, markerRecorded,
                 observerKnown, recoveryObservation, rootPresent, resourceEffect, deletion,
                 handQuiescent, sessionReceipt>>

AcquireRecovery(operation) ==
  /\ cleanupCaller = NoLock /\ UNCHANGED cleanupVars
  /\ markerLock = "free" /\ markerPhase # "removed"
  /\ (operation = "remove" \/ markerPhase = "ready")
  /\ markerLock' = operation
  /\ markerPhase' = IF operation = "remove" THEN "removing" ELSE markerPhase
  /\ recoveryObservation' = "none"
  /\ UNCHANGED <<allBoundaryVars, renewalVars, markerRecorded, observerKnown,
                 rootPresent, resourceEffect, deletion, handQuiescent, sessionReceipt>>

ObserveRecovery ==
  /\ UNCHANGED cleanupVars
  /\ markerLock \in {"resource", "remove"} /\ recoveryObservation = "none"
  /\ recoveryObservation' =
       IF ~markerRecorded THEN "empty"
       ELSE IF ~observerKnown THEN "unknown"
       ELSE IF initState \in {"absent", "exited"} THEN "empty" ELSE "live"
  /\ UNCHANGED <<allBoundaryVars, renewalVars, markerPhase, markerRecorded,
                 markerLock, observerKnown, rootPresent, resourceEffect, deletion,
                 handQuiescent, sessionReceipt>>

RecoveryRetry ==
  /\ cleanupCaller = NoLock /\ UNCHANGED cleanupVars
  /\ markerLock \in {"resource", "remove"}
  /\ recoveryObservation \in {"unknown", "live"}
  /\ markerLock' = "free" /\ recoveryObservation' = "none"
  /\ UNCHANGED <<allBoundaryVars, renewalVars, markerPhase, markerRecorded,
                 observerKnown, rootPresent, resourceEffect, deletion,
                 handQuiescent, sessionReceipt>>

\* A changed boot/observer can only remove knowledge; it is never a stop receipt.
LoseObserver ==
  /\ UNCHANGED cleanupVars
  /\ observerKnown /\ observerKnown' = FALSE
  /\ recoveryObservation' = IF recoveryObservation = "empty" /\ markerRecorded
                             THEN "unknown" ELSE recoveryObservation
  /\ UNCHANGED <<allBoundaryVars, renewalVars, markerPhase, markerRecorded,
                 markerLock, rootPresent, resourceEffect, deletion,
                 handQuiescent, sessionReceipt>>

RecoveryQuiescent ==
  /\ recoveryObservation = "empty"
  /\ handQuiescent

ApplyRecovery ==
  /\ markerLock = "resource" /\ RecoveryQuiescent
  /\ resourceEffect' = TRUE
  /\ markerLock' = "free" /\ recoveryObservation' = "none"
  /\ UNCHANGED <<allBoundaryVars, renewalVars, markerRecorded, observerKnown,
                 handQuiescent, sessionReceipt, rootPresent, markerPhase,
                 deletion, cleanupVars>>

\* Refine the former atomic remove action using the SAME marker/file-lock
\* authority. Waiters share one resident Option<RemovalGuard>; cancellation
\* releases its async borrow, not the prepared guard. Valid/expired input is
\* abstracted separately from the GO authority: no cross-clock or Session
\* revocation refinement is claimed. Async effects may complete after expiry;
\* the next effect must revalidate. There is no unconditional liveness claim.
EnterCleanup(c, operation) ==
  /\ markerLock = "remove" /\ cleanupCaller = NoLock
  /\ RecoveryQuiescent /\ cleanupFenceLive
  /\ cleanupCaller' = c /\ cleanupStage' = operation
  /\ cleanupQueued' = cleanupQueued \ {c}
  /\ UNCHANGED <<allBoundaryVars, renewalVars, markerVars,
                 cleanupFenceLive, controlQuiescent, mountsReleased>>

QueueCleanup(c) ==
  /\ cleanupCaller \in Waiters /\ c # cleanupCaller
  /\ c \notin cleanupQueued /\ cleanupQueued' = cleanupQueued \cup {c}
  /\ UNCHANGED <<allBoundaryVars, renewalVars, markerVars, cleanupCaller,
                 cleanupStage, cleanupFenceLive, controlQuiescent, mountsReleased>>

CancelCleanup ==
  /\ cleanupCaller \in Waiters
  /\ cleanupCaller' = NoLock /\ cleanupStage' = "idle"
  /\ UNCHANGED <<allBoundaryVars, renewalVars, markerVars,
                 cleanupFenceLive, controlQuiescent, mountsReleased, cleanupQueued>>

CompleteCheckpoint ==
  /\ cleanupCaller \in Waiters /\ cleanupStage = "checkpoint"
  /\ cleanupCaller' = NoLock /\ cleanupStage' = "idle"
  /\ UNCHANGED <<allBoundaryVars, renewalVars, markerVars,
                 cleanupFenceLive, controlQuiescent, mountsReleased, cleanupQueued>>

CompleteControl ==
  /\ cleanupCaller \in Waiters /\ cleanupStage = "control"
  /\ controlQuiescent' = TRUE /\ cleanupStage' = "memory-admit"
  /\ UNCHANGED <<allBoundaryVars, renewalVars, markerVars, cleanupCaller,
                 cleanupFenceLive, mountsReleased, cleanupQueued>>

BeginMountRelease ==
  /\ cleanupCaller \in Waiters /\ cleanupStage \in {"memory-admit", "memory-next"}
  /\ cleanupFenceLive /\ RecoveryQuiescent
  /\ cleanupStage' = "memory"
  /\ UNCHANGED <<allBoundaryVars, renewalVars, markerVars, cleanupCaller,
                 cleanupFenceLive, controlQuiescent, mountsReleased, cleanupQueued>>

\* Abstract one or more sequential mounts without a second mount registry.
\* Waiting for the list lock and each next teardown crosses this same admission.
\* A started operation may complete after expiry, but its success cannot admit
\* another mount or release the retained set. Rust tests instantiate two mounts.
CompleteMountRelease(last) ==
  /\ cleanupCaller \in Waiters /\ cleanupStage = "memory"
  /\ cleanupStage' = IF last THEN "memory-finish" ELSE "memory-next"
  /\ UNCHANGED <<allBoundaryVars, renewalVars, markerVars, cleanupCaller,
                 cleanupFenceLive, controlQuiescent, mountsReleased, cleanupQueued>>

FinishMountSet ==
  /\ cleanupCaller \in Waiters /\ cleanupStage = "memory-finish"
  /\ cleanupFenceLive /\ RecoveryQuiescent
  /\ mountsReleased' = TRUE /\ cleanupStage' = "physical"
  /\ UNCHANGED <<allBoundaryVars, renewalVars, markerVars, cleanupCaller,
                 cleanupFenceLive, controlQuiescent, cleanupQueued>>

ApplyRemoval ==
  /\ cleanupCaller \in Waiters /\ cleanupStage = "physical"
  /\ cleanupFenceLive /\ RecoveryQuiescent
  /\ rootPresent' = FALSE /\ deletion' = TRUE /\ cleanupStage' = "finish"
  /\ UNCHANGED <<allBoundaryVars, renewalVars, markerPhase, markerRecorded,
                 markerLock, observerKnown, recoveryObservation, resourceEffect,
                 handQuiescent, sessionReceipt, cleanupCaller, cleanupFenceLive,
                 controlQuiescent, mountsReleased, cleanupQueued>>

\* Publication and acknowledgement are separate: cancellation at either cut
\* must leave exact replay possible, including root-absent/Removing and an
\* applied Removed marker whose completion response was lost.
PublishRemoved ==
  /\ cleanupCaller \in Waiters /\ cleanupStage = "finish"
  /\ cleanupFenceLive /\ ~rootPresent
  /\ markerPhase' = "removed" /\ cleanupStage' = "receipt"
  /\ UNCHANGED <<allBoundaryVars, renewalVars, markerRecorded, markerLock,
                 observerKnown, recoveryObservation, rootPresent, resourceEffect,
                 deletion, handQuiescent, sessionReceipt, cleanupCaller,
                 cleanupFenceLive, controlQuiescent, mountsReleased, cleanupQueued>>

FinishRemoval ==
  /\ cleanupCaller \in Waiters /\ cleanupStage = "receipt"
  /\ markerPhase = "removed" /\ ~rootPresent
  /\ markerLock' = "free" /\ cleanupCaller' = NoLock /\ cleanupStage' = "idle"
  /\ UNCHANGED <<allBoundaryVars, renewalVars, markerPhase, markerRecorded,
                 observerKnown, recoveryObservation, rootPresent, resourceEffect,
                 deletion, handQuiescent, sessionReceipt, cleanupFenceLive,
                 controlQuiescent, mountsReleased, cleanupQueued>>

ObserveCleanupFence ==
  /\ cleanupFenceLive' = ~cleanupFenceLive
  /\ UNCHANGED <<allBoundaryVars, renewalVars, markerVars, cleanupCaller,
                 cleanupStage, controlQuiescent, mountsReleased, cleanupQueued>>

\* The existing Session Hand lifecycle independently closes admission and
\* observes exact reap (or proves no binding). Unknown/failure remains FALSE;
\* it cannot prevent the independent OS boundary from progressing to exit.
\* No fairness assumption promises Hand reap, procfs recovery or bounded stop.
ObserveHandQuiescence ==
  /\ UNCHANGED cleanupVars
  /\ scopeClosed /\ ~handQuiescent /\ handQuiescent' = TRUE
  /\ UNCHANGED <<allBoundaryVars, renewalVars, markerPhase, markerRecorded,
                 markerLock, observerKnown, recoveryObservation, rootPresent,
                 resourceEffect, deletion, sessionReceipt>>

ReceiveSessionStop ==
  /\ UNCHANGED cleanupVars
  /\ scopeClosed /\ markerLock = "free" /\ ~sessionReceipt
  /\ handQuiescent
  /\ (~markerRecorded \/ (observerKnown /\ initState \in {"absent", "exited"}))
  /\ sessionReceipt' = TRUE
  /\ UNCHANGED <<allBoundaryVars, renewalVars, markerPhase, markerRecorded,
                 markerLock, observerKnown, recoveryObservation, rootPresent,
                 resourceEffect, deletion, handQuiescent>>

RecoveryNext == RecoveryBoundary \/ PersistBeforeGo \/ ReleaseLaunch
  \/ (\E operation \in {"resource", "remove"} : AcquireRecovery(operation))
  \/ ObserveRecovery \/ RecoveryRetry \/ LoseObserver \/ ApplyRecovery
  \/ ObserveHandQuiescence \/ ReceiveSessionStop
  \/ (\E c \in Waiters: QueueCleanup(c)
        \/ (\E operation \in {"checkpoint", "control", "finish"}: EnterCleanup(c, operation)))
  \/ CancelCleanup \/ CompleteCheckpoint \/ CompleteControl \/ BeginMountRelease
  \/ (\E last \in BOOLEAN: CompleteMountRelease(last)) \/ FinishMountSet
  \/ ApplyRemoval \/ PublishRemoved \/ FinishRemoval
  \/ ObserveCleanupFence

RecoveryTypeOK == BoundaryTypeOK
  /\ markerPhase \in {"ready", "removing", "removed"}
  /\ markerRecorded \in BOOLEAN /\ observerKnown \in BOOLEAN
  /\ markerLock \in {"free", "launch", "resource", "remove"}
  /\ recoveryObservation \in {"none", "empty", "live", "unknown"}
  /\ rootPresent \in BOOLEAN /\ resourceEffect \in BOOLEAN /\ deletion \in BOOLEAN
  /\ handQuiescent \in BOOLEAN /\ sessionReceipt \in BOOLEAN
  /\ cleanupCaller \in Waiters \cup {NoLock}
  /\ cleanupStage \in {"idle", "checkpoint", "control", "memory-admit", "memory-next", "memory", "memory-finish", "physical", "finish", "receipt"}
  /\ cleanupFenceLive \in BOOLEAN /\ controlQuiescent \in BOOLEAN
  /\ mountsReleased \in BOOLEAN /\ cleanupQueued \subseteq Waiters
GoRequiresDurableParticipant == goIssued => markerRecorded
LaunchOwnsMarker == scopePermit => markerLock = "launch" /\ markerPhase = "ready"
RemovedCannotExecute == deletion => (~workloadStarted \/ initState = "exited")
RemovedIsPhysical ==
  /\ markerPhase = "removed" => ~rootPresent
  /\ ~rootPresent => markerPhase \in {"removing", "removed"}
CleanupBorrowRetainsMarker == cleanupCaller \in Waiters => markerLock = "remove"
RootRemovalFollowsParticipants == deletion => controlQuiescent /\ mountsReleased
CleanupCancellationRetainsParticipant ==
  [][(cleanupCaller \in Waiters /\ cleanupCaller' = NoLock
      /\ cleanupStage \notin {"receipt"} /\ phase' = phase)
     => markerLock' = "remove"]_allVars
CleanupBorrowIsExclusive ==
  [][(cleanupCaller \in Waiters) => cleanupCaller' \in {cleanupCaller, NoLock}]_allVars
CleanupEffectRequiresFreshFence ==
  [][(cleanupStage' \in {"control", "checkpoint", "memory", "physical", "finish", "receipt"}
      /\ cleanupStage' # cleanupStage) => cleanupFenceLive]_allVars
NoCancelledCleanupReached == ~(cleanupCaller = NoLock /\ markerLock = "remove" /\ controlQuiescent)
NoQueuedCheckpointReached == ~(cleanupStage = "checkpoint" /\ cleanupQueued # {})
NoUnrecordedRemovalReached == ~(~rootPresent /\ markerPhase = "removing")
NoLostRemovedReceiptReached == ~(markerPhase = "removed" /\ markerLock = "remove" /\ cleanupCaller = NoLock)
NoInterMountAdmissionReached == cleanupStage # "memory-next"
EffectsRequireKnownQuiescence ==
  [][((~resourceEffect /\ resourceEffect') \/ (~deletion /\ deletion'))
     => recoveryObservation = "empty"
        /\ (~markerRecorded \/ (observerKnown /\ initState \in {"absent", "exited"}))]_allVars
CrashRetainsDurableParticipant ==
  [][(phase # "crashed" /\ phase' = "crashed") => UNCHANGED markerRecorded]_allVars
SessionReceiptRequiresAllParticipants ==
  [][(~sessionReceipt /\ sessionReceipt') =>
     scopeClosed /\ ~scopePermit /\ handQuiescent
     /\ (~markerRecorded \/ (observerKnown /\ initState \in {"absent", "exited"}))]_allVars
EffectsRequireHandQuiescence ==
  [][((~resourceEffect /\ resourceEffect') \/ (~deletion /\ deletion'))
     => handQuiescent]_allVars
RecoverySpec == RecoveryInit /\ [][RecoveryNext]_allVars
=============================================================================
