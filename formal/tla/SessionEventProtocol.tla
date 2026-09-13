----------------------- MODULE SessionEventProtocol -----------------------
EXTENDS Naturals

CONSTANTS EventOperation, RunOperation, Workers, NoWorker, MaxEpoch, ObservationTimes, UnknownTime

VARIABLES dispatchExists, dispatchState, dispatchOwner, dispatchEpoch,
          dispatchCancel, dispatchInput, activityEpoch, activeActivityEpochs,
          settledActivities, nextActivityEpoch, eventPhase,
          committedDisposition, projectionAnchor, processedAt,
          queuedUser, intervalOpen, legacyClosure

vars == <<dispatchExists, dispatchState, dispatchOwner, dispatchEpoch,
          dispatchCancel, dispatchInput, activityEpoch, activeActivityEpochs,
          settledActivities, nextActivityEpoch, eventPhase,
          committedDisposition, projectionAnchor, processedAt,
          queuedUser, intervalOpen, legacyClosure>>

coreVars == <<dispatchExists, dispatchState, dispatchOwner, dispatchEpoch,
              dispatchCancel, dispatchInput, activityEpoch, activeActivityEpochs,
              settledActivities, nextActivityEpoch, eventPhase,
              committedDisposition, projectionAnchor, processedAt>>

Kernel == INSTANCE SessionEventKernel WITH
    EventOperation <- EventOperation,
    RunOperation <- RunOperation,
    Workers <- Workers,
    NoWorker <- NoWorker,
    MaxEpoch <- MaxEpoch,
    kDispatchExists <- dispatchExists,
    kDispatchState <- dispatchState,
    kDispatchOwner <- dispatchOwner,
    kDispatchEpoch <- dispatchEpoch,
    kDispatchCancel <- dispatchCancel,
    kDispatchInput <- dispatchInput,
    kActivityEpoch <- activityEpoch,
    kActiveActivityEpochs <- activeActivityEpochs,
    kSettledActivities <- settledActivities,
    kNextActivityEpoch <- nextActivityEpoch,
    kEventPhase <- eventPhase,
    kCommittedDisposition <- committedDisposition,
    kProjectionAnchor <- projectionAnchor

ASSUME /\ ObservationTimes \subseteq Nat
       /\ ObservationTimes /= {}
       /\ UnknownTime \notin ObservationTimes
Init == /\ Kernel!Init /\ processedAt = UnknownTime
        /\ queuedUser = FALSE /\ intervalOpen = FALSE
        /\ legacyClosure \in BOOLEAN
ProcessedObservationTime(wasProcessed, isProcessed, before, now) ==
    IF ~wasProcessed /\ isProcessed THEN now ELSE before

ProcessedObservationStep(before, after, now) ==
    /\ (before.processed => after.processed)
    /\ after.time = ProcessedObservationTime(before.processed, after.processed, before.time, now)

\* AQ1-AQ4: queue a later User before/after the predecessor boundary. This
\* wrapper models its blocked admission only; it cannot mutate the existing
\* Dispatch/activity kernels. User+paired System has the same abstraction.
\* legacyClosure explores the previously persisted Running shape; repair uses
\* the same exact processed/settled causal chain, not elapsed time or leases.
BoundarySettled == eventPhase = "Processed" /\ activeActivityEpochs = {}
KernelStep == /\ Kernel!Next
        /\ \E now \in ObservationTimes :
             processedAt' = ProcessedObservationTime(eventPhase = "Processed", eventPhase' = "Processed", processedAt, now)
        /\ intervalOpen' = (IF eventPhase' = "Processed" /\ activeActivityEpochs' = {}
                              /\ (~queuedUser \/ (committedDisposition' = "Awaiting" /\ ~legacyClosure))
                            THEN FALSE
                            ELSE IF activeActivityEpochs' # {} THEN TRUE ELSE intervalOpen)
        /\ UNCHANGED <<queuedUser, legacyClosure>>
QueueUser == /\ ~queuedUser /\ eventPhase \in {"Activated", "Anchored", "Processed"}
             /\ queuedUser' = TRUE
             /\ UNCHANGED <<coreVars, intervalOpen, legacyClosure>>
RecoverAwaitingInterval == /\ intervalOpen /\ BoundarySettled
                           /\ committedDisposition = "Awaiting"
                           /\ intervalOpen' = FALSE
                           /\ UNCHANGED <<coreVars, queuedUser, legacyClosure>>
Next == KernelStep \/ QueueUser \/ RecoverAwaitingInterval
Spec == Init /\ [][Next]_vars /\ WF_vars(RecoverAwaitingInterval)
Safety == /\ Kernel!Safety
          /\ ((eventPhase = "Processed") <=> (processedAt \in ObservationTimes))
          /\ queuedUser \in BOOLEAN /\ intervalOpen \in BOOLEAN /\ legacyClosure \in BOOLEAN
          /\ (eventPhase \in {"Activated", "Anchored"} => intervalOpen)
FirstProcessingTimeIsImmutable == [][processedAt # UnknownTime => processedAt' = processedAt]_vars
AwaitingActionEventuallyVisible ==
    (BoundarySettled /\ committedDisposition = "Awaiting") ~> ~intervalOpen
RecoveryPreservesAuthorities == [][RecoverAwaitingInterval => UNCHANGED coreVars]_vars

=============================================================================
