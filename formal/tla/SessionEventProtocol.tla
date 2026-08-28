----------------------- MODULE SessionEventProtocol -----------------------
EXTENDS Naturals

CONSTANTS EventOperation, RunOperation, Workers, NoWorker, MaxEpoch

VARIABLES dispatchExists, dispatchState, dispatchOwner, dispatchEpoch,
          dispatchCancel, dispatchInput, activityEpoch, activeActivityEpochs,
          settledActivities, nextActivityEpoch, eventPhase,
          committedDisposition, projectionAnchor

vars == <<dispatchExists, dispatchState, dispatchOwner, dispatchEpoch,
          dispatchCancel, dispatchInput, activityEpoch, activeActivityEpochs,
          settledActivities, nextActivityEpoch, eventPhase,
          committedDisposition, projectionAnchor>>

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

Init == Kernel!Init
Next == Kernel!Next
Spec == Init /\ [][Next]_vars
Safety == Kernel!Safety

=============================================================================
