---------------------- MODULE LocalProcessRecovery ----------------------
EXTENDS LocalProcessLifecycle

\* Compose, do not duplicate, the existing GO/current-authority/kernel model.
\* One execution, one Ready realization and two competing recovery entrances.
\* Session authorization, marker atomic replacement and truthful OS evidence
\* are assumptions. This is not a Rust refinement or kernel correctness proof.
VARIABLES markerPhase, markerRecorded, markerLock, observerKnown,
          recoveryObservation, rootPresent, resourceEffect, deletion
markerVars == <<markerPhase, markerRecorded, markerLock, observerKnown,
               recoveryObservation, rootPresent, resourceEffect, deletion>>
allVars == <<allBoundaryVars, renewalVars, markerVars>>

RecoveryInit ==
  /\ Init
  /\ markerPhase = "ready" /\ markerRecorded = FALSE /\ markerLock = "free"
  /\ observerKnown = TRUE /\ recoveryObservation = "none"
  /\ rootPresent = TRUE /\ resourceEffect = FALSE /\ deletion = FALSE

RecoveryBoundary ==
  /\ BoundaryNext /\ UNCHANGED renewalVars
  /\ ((~scopePermit /\ scopePermit') =>
         markerLock = "free" /\ markerPhase = "ready" /\ rootPresent)
  /\ ((~goIssued /\ goIssued') => markerRecorded /\ markerLock = "launch")
  /\ markerLock' = IF ~scopePermit /\ scopePermit' THEN "launch"
                    ELSE IF phase # "crashed" /\ phase' = "crashed"
                         THEN "free" ELSE markerLock
  /\ UNCHANGED <<markerPhase, markerRecorded, observerKnown,
                 recoveryObservation, rootPresent, resourceEffect, deletion>>

PersistBeforeGo ==
  /\ markerLock = "launch" /\ authorityHeld /\ launchStage = "acknowledged"
  /\ ~markerRecorded /\ markerRecorded' = TRUE
  /\ UNCHANGED <<allBoundaryVars, renewalVars, markerPhase, markerLock,
                 observerKnown, recoveryObservation, rootPresent, resourceEffect, deletion>>

ReleaseLaunch ==
  /\ markerLock = "launch" /\ ~scopePermit
  /\ markerLock' = "free"
  /\ UNCHANGED <<allBoundaryVars, renewalVars, markerPhase, markerRecorded,
                 observerKnown, recoveryObservation, rootPresent, resourceEffect, deletion>>

AcquireRecovery(operation) ==
  /\ markerLock = "free" /\ markerPhase # "removed"
  /\ (operation = "remove" \/ markerPhase = "ready")
  /\ markerLock' = operation
  /\ markerPhase' = IF operation = "remove" THEN "removing" ELSE markerPhase
  /\ recoveryObservation' = "none"
  /\ UNCHANGED <<allBoundaryVars, renewalVars, markerRecorded, observerKnown,
                 rootPresent, resourceEffect, deletion>>

ObserveRecovery ==
  /\ markerLock \in {"resource", "remove"} /\ recoveryObservation = "none"
  /\ recoveryObservation' =
       IF ~markerRecorded THEN "empty"
       ELSE IF ~observerKnown THEN "unknown"
       ELSE IF initState \in {"absent", "exited"} THEN "empty" ELSE "live"
  /\ UNCHANGED <<allBoundaryVars, renewalVars, markerPhase, markerRecorded,
                 markerLock, observerKnown, rootPresent, resourceEffect, deletion>>

RecoveryRetry ==
  /\ markerLock \in {"resource", "remove"}
  /\ recoveryObservation \in {"unknown", "live"}
  /\ markerLock' = "free" /\ recoveryObservation' = "none"
  /\ UNCHANGED <<allBoundaryVars, renewalVars, markerPhase, markerRecorded,
                 observerKnown, rootPresent, resourceEffect, deletion>>

\* A changed boot/observer can only remove knowledge; it is never a stop receipt.
LoseObserver ==
  /\ observerKnown /\ observerKnown' = FALSE
  /\ recoveryObservation' = IF recoveryObservation = "empty" /\ markerRecorded
                             THEN "unknown" ELSE recoveryObservation
  /\ UNCHANGED <<allBoundaryVars, renewalVars, markerPhase, markerRecorded,
                 markerLock, rootPresent, resourceEffect, deletion>>

ApplyRecovery ==
  /\ markerLock \in {"resource", "remove"} /\ recoveryObservation = "empty"
  /\ resourceEffect' = (resourceEffect \/ markerLock = "resource")
  /\ deletion' = (deletion \/ markerLock = "remove")
  /\ rootPresent' = IF markerLock = "remove" THEN FALSE ELSE rootPresent
  /\ markerPhase' = IF markerLock = "remove" THEN "removed" ELSE markerPhase
  /\ markerLock' = "free" /\ recoveryObservation' = "none"
  /\ UNCHANGED <<allBoundaryVars, renewalVars, markerRecorded, observerKnown>>

RecoveryNext == RecoveryBoundary \/ PersistBeforeGo \/ ReleaseLaunch
  \/ (\E operation \in {"resource", "remove"} : AcquireRecovery(operation))
  \/ ObserveRecovery \/ RecoveryRetry \/ LoseObserver \/ ApplyRecovery

RecoveryTypeOK == BoundaryTypeOK
  /\ markerPhase \in {"ready", "removing", "removed"}
  /\ markerRecorded \in BOOLEAN /\ observerKnown \in BOOLEAN
  /\ markerLock \in {"free", "launch", "resource", "remove"}
  /\ recoveryObservation \in {"none", "empty", "live", "unknown"}
  /\ rootPresent \in BOOLEAN /\ resourceEffect \in BOOLEAN /\ deletion \in BOOLEAN
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
RecoverySpec == RecoveryInit /\ [][RecoveryNext]_allVars
=============================================================================
