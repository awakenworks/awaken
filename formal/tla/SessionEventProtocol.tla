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
ProcessedObservationTime(wasProcessed, isProcessed, before, now) ==
    IF ~wasProcessed /\ isProcessed THEN now ELSE before

ProcessedObservationStep(before, after, now) ==
    /\ (before.processed => after.processed)
    /\ after.time = ProcessedObservationTime(before.processed, after.processed, before.time, now)

Next == /\ Kernel!Next
        /\ \E now \in ObservationTimes :
             processedAt' = ProcessedObservationTime(eventPhase = "Processed", eventPhase' = "Processed", processedAt, now)
Spec == Init /\ [][Next]_vars
Safety == /\ Kernel!Safety
          /\ ((eventPhase = "Processed") <=> (processedAt \in ObservationTimes))
FirstProcessingTimeIsImmutable == [][processedAt # UnknownTime => processedAt' = processedAt]_vars

=============================================================================
