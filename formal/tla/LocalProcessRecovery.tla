---------------------- MODULE LocalProcessRecovery ----------------------
EXTENDS LocalProcessLifecycle

\* Compose, do not duplicate, the existing GO/current-authority/kernel model.
\* One execution, one Ready realization and two competing recovery entrances.
\* Session authorization, marker atomic replacement and truthful OS evidence
\* are assumptions. This is not a Rust refinement or kernel correctness proof.
VARIABLES markerPhase, markerRecorded, markerLock, observerKnown,
          recoveryObservation, rootPresent, resourceEffect, deletion,
          handQuiescent, sessionReceipt
markerVars == <<markerPhase, markerRecorded, markerLock, observerKnown,
               recoveryObservation, rootPresent, resourceEffect, deletion,
               handQuiescent, sessionReceipt>>
allVars == <<allBoundaryVars, renewalVars, markerVars>>

RecoveryInit ==
  /\ Init
  /\ markerPhase = "ready" /\ markerRecorded = FALSE /\ markerLock = "free"
  /\ observerKnown = TRUE /\ recoveryObservation = "none"
  /\ rootPresent = TRUE /\ resourceEffect = FALSE /\ deletion = FALSE
  /\ handQuiescent = FALSE /\ sessionReceipt = FALSE

RecoveryBoundary ==
  /\ BoundaryNext /\ UNCHANGED renewalVars
  /\ ((~scopePermit /\ scopePermit') =>
         markerLock = "free" /\ markerPhase = "ready" /\ rootPresent)
  /\ ((~goIssued /\ goIssued') => markerRecorded /\ markerLock = "launch")
  /\ markerLock' = IF ~scopePermit /\ scopePermit' THEN "launch"
                    ELSE IF phase # "crashed" /\ phase' = "crashed"
                         THEN "free" ELSE markerLock
  /\ UNCHANGED <<markerPhase, markerRecorded, observerKnown,
                 recoveryObservation, rootPresent, resourceEffect, deletion,
                 handQuiescent, sessionReceipt>>

PersistBeforeGo ==
  /\ markerLock = "launch" /\ authorityHeld /\ launchStage = "acknowledged"
  /\ ~markerRecorded /\ markerRecorded' = TRUE
  /\ UNCHANGED <<allBoundaryVars, renewalVars, markerPhase, markerLock,
                 observerKnown, recoveryObservation, rootPresent, resourceEffect, deletion,
                 handQuiescent, sessionReceipt>>

ReleaseLaunch ==
  /\ markerLock = "launch" /\ ~scopePermit
  /\ markerLock' = "free"
  /\ UNCHANGED <<allBoundaryVars, renewalVars, markerPhase, markerRecorded,
                 observerKnown, recoveryObservation, rootPresent, resourceEffect, deletion,
                 handQuiescent, sessionReceipt>>

AcquireRecovery(operation) ==
  /\ markerLock = "free" /\ markerPhase # "removed"
  /\ (operation = "remove" \/ markerPhase = "ready")
  /\ markerLock' = operation
  /\ markerPhase' = IF operation = "remove" THEN "removing" ELSE markerPhase
  /\ recoveryObservation' = "none"
  /\ UNCHANGED <<allBoundaryVars, renewalVars, markerRecorded, observerKnown,
                 rootPresent, resourceEffect, deletion, handQuiescent, sessionReceipt>>

ObserveRecovery ==
  /\ markerLock \in {"resource", "remove"} /\ recoveryObservation = "none"
  /\ recoveryObservation' =
       IF ~markerRecorded THEN "empty"
       ELSE IF ~observerKnown THEN "unknown"
       ELSE IF initState \in {"absent", "exited"} THEN "empty" ELSE "live"
  /\ UNCHANGED <<allBoundaryVars, renewalVars, markerPhase, markerRecorded,
                 markerLock, observerKnown, rootPresent, resourceEffect, deletion,
                 handQuiescent, sessionReceipt>>

RecoveryRetry ==
  /\ markerLock \in {"resource", "remove"}
  /\ recoveryObservation \in {"unknown", "live"}
  /\ markerLock' = "free" /\ recoveryObservation' = "none"
  /\ UNCHANGED <<allBoundaryVars, renewalVars, markerPhase, markerRecorded,
                 observerKnown, rootPresent, resourceEffect, deletion,
                 handQuiescent, sessionReceipt>>

\* A changed boot/observer can only remove knowledge; it is never a stop receipt.
LoseObserver ==
  /\ observerKnown /\ observerKnown' = FALSE
  /\ recoveryObservation' = IF recoveryObservation = "empty" /\ markerRecorded
                             THEN "unknown" ELSE recoveryObservation
  /\ UNCHANGED <<allBoundaryVars, renewalVars, markerPhase, markerRecorded,
                 markerLock, rootPresent, resourceEffect, deletion,
                 handQuiescent, sessionReceipt>>

ApplyRecovery ==
  /\ markerLock \in {"resource", "remove"} /\ recoveryObservation = "empty"
  /\ handQuiescent
  /\ resourceEffect' = (resourceEffect \/ markerLock = "resource")
  /\ deletion' = (deletion \/ markerLock = "remove")
  /\ rootPresent' = IF markerLock = "remove" THEN FALSE ELSE rootPresent
  /\ markerPhase' = IF markerLock = "remove" THEN "removed" ELSE markerPhase
  /\ markerLock' = "free" /\ recoveryObservation' = "none"
  /\ UNCHANGED <<allBoundaryVars, renewalVars, markerRecorded, observerKnown,
                 handQuiescent, sessionReceipt>>

\* The existing Session Hand lifecycle independently closes admission and
\* observes exact reap (or proves no binding). Unknown/failure remains FALSE;
\* it cannot prevent the independent OS boundary from progressing to exit.
\* No fairness assumption promises Hand reap, procfs recovery or bounded stop.
ObserveHandQuiescence ==
  /\ scopeClosed /\ ~handQuiescent /\ handQuiescent' = TRUE
  /\ UNCHANGED <<allBoundaryVars, renewalVars, markerPhase, markerRecorded,
                 markerLock, observerKnown, recoveryObservation, rootPresent,
                 resourceEffect, deletion, sessionReceipt>>

ReceiveSessionStop ==
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

RecoveryTypeOK == BoundaryTypeOK
  /\ markerPhase \in {"ready", "removing", "removed"}
  /\ markerRecorded \in BOOLEAN /\ observerKnown \in BOOLEAN
  /\ markerLock \in {"free", "launch", "resource", "remove"}
  /\ recoveryObservation \in {"none", "empty", "live", "unknown"}
  /\ rootPresent \in BOOLEAN /\ resourceEffect \in BOOLEAN /\ deletion \in BOOLEAN
  /\ handQuiescent \in BOOLEAN /\ sessionReceipt \in BOOLEAN
GoRequiresDurableParticipant == goIssued => markerRecorded
LaunchOwnsMarker == scopePermit => markerLock = "launch" /\ markerPhase = "ready"
RemovedCannotExecute == deletion => (~workloadStarted \/ initState = "exited")
RemovedIsPhysical == (markerPhase = "removed") = ~rootPresent
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
