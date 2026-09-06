----------------------- MODULE SessionEventProtocol -----------------------
EXTENDS Naturals

CONSTANTS EventOperation, RunOperation, Workers, NoWorker, MaxEpoch, ObservationTimes, UnknownTime

VARIABLES dispatchExists, dispatchState, dispatchOwner, dispatchEpoch,
          dispatchCancel, dispatchInput, activityEpoch, activeActivityEpochs,
          settledActivities, nextActivityEpoch, eventPhase,
          committedDisposition, projectionAnchor, processedAt

vars == <<dispatchExists, dispatchState, dispatchOwner, dispatchEpoch,
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
Next == /\ Kernel!Next
        /\ \E now \in ObservationTimes :
             processedAt' = IF eventPhase # "Processed" /\ eventPhase' = "Processed"
                            THEN now ELSE processedAt
Spec == Init /\ [][Next]_vars
Safety == /\ Kernel!Safety
          /\ ((eventPhase = "Processed") <=> (processedAt \in ObservationTimes))
FirstProcessingTimeIsImmutable == [][processedAt # UnknownTime => processedAt' = processedAt]_vars

=============================================================================
