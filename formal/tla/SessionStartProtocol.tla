------------------------ MODULE SessionStartProtocol ------------------------
EXTENDS Naturals

\* End-to-end composition for the bounded initial-Event path:
\* atomic Session root create -> disposable Work projection -> exact Work
\* lease -> fenced realization -> Event Run reservation/activity/activation ->
\* Dispatch claim -> observed ThreadCommit -> Session settlement -> Dispatch
\* settlement -> Event settlement -> exact Work release.
\*
\* The three component transition relations are instantiated, never copied.
CONSTANTS SessionOwner, Workers, NoActor, Payload, WorkItem,
          EventOperation, RunOperation, MaxRevision, MaxEpoch,
          MaxHeartbeat, MaxTime

Actors == Workers \cup {SessionOwner}
Payloads == {Payload}
WorkItems == {WorkItem}

VARIABLES
    existence, owner, createPayload, createReceipt, baseline, execution,
    placement, revision, hasInitialEvent, realizationState, realizationOwner,
    realizationEpoch, realizationEffects, activationCommitted, workProjected,
    everTerminal, lastCreateOutcome,
    dispatchExists, dispatchState, dispatchOwner, dispatchEpoch,
    dispatchCancel, dispatchInput, activityEpoch, activeActivityEpochs,
    settledActivities, nextActivityEpoch, eventPhase, committedDisposition,
    projectionAnchor,
    workRowPresent, workState, workOwner, workEpoch, workHeartbeat, workExpires,
    workNow, realizationWorkEpoch

rootVars == <<existence, owner, createPayload, createReceipt, baseline,
              execution, placement, revision, hasInitialEvent,
              realizationState, realizationOwner, realizationEpoch,
              realizationEffects, activationCommitted, workProjected,
              everTerminal, lastCreateOutcome>>
eventVars == <<dispatchExists, dispatchState, dispatchOwner, dispatchEpoch,
               dispatchCancel, dispatchInput, activityEpoch,
               activeActivityEpochs, settledActivities, nextActivityEpoch,
               eventPhase, committedDisposition, projectionAnchor>>
workVars == <<workState, workOwner, workEpoch, workHeartbeat, workExpires,
              workNow>>
vars == <<rootVars, eventVars, workRowPresent, workVars, realizationWorkEpoch>>

Root == INSTANCE SessionRootKernel WITH
    Owners <- Actors,
    NoOwner <- NoActor,
    Payloads <- Payloads,
    NoPayload <- NoActor,
    MaxRevision <- MaxRevision,
    MaxRealizationEpoch <- MaxEpoch,
    kExistence <- existence,
    kOwner <- owner,
    kCreatePayload <- createPayload,
    kCreateReceipt <- createReceipt,
    kBaseline <- baseline,
    kExecution <- execution,
    kPlacement <- placement,
    kRevision <- revision,
    kHasInitialEvent <- hasInitialEvent,
    kRealizationState <- realizationState,
    kRealizationOwner <- realizationOwner,
    kRealizationEpoch <- realizationEpoch,
    kRealizationEffects <- realizationEffects,
    kActivationCommitted <- activationCommitted,
    kWorkProjected <- workProjected,
    kEverTerminal <- everTerminal,
    kLastCreateOutcome <- lastCreateOutcome

Event == INSTANCE SessionEventKernel WITH
    EventOperation <- EventOperation,
    RunOperation <- RunOperation,
    Workers <- Workers,
    NoWorker <- NoActor,
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

WorkKernel == INSTANCE WorkQueueKernel WITH
    WorkItems <- WorkItems,
    Workers <- Workers,
    NoWorker <- NoActor,
    MaxEpoch <- MaxEpoch,
    MaxHeartbeat <- MaxHeartbeat,
    MaxTime <- MaxTime,
    kState <- workState,
    kOwner <- workOwner,
    kEpoch <- workEpoch,
    kHeartbeat <- workHeartbeat,
    kExpires <- workExpires,
    kNow <- workNow

Init ==
    /\ Root!Init
    /\ Event!Init
    /\ WorkKernel!Init
    /\ workRowPresent = FALSE
    /\ realizationWorkEpoch = 0

CurrentWork(worker) ==
    /\ workRowPresent
    /\ workProjected
    /\ workState[WorkItem] = "Active"
    /\ workOwner[WorkItem] = worker

CurrentRealizationWork(driver) ==
    /\ CurrentWork(driver)
    /\ workEpoch[WorkItem] = realizationWorkEpoch

SessionExecutable ==
    /\ existence = "Live"
    /\ baseline = "Frozen"
    /\ execution = "Running"
    /\ realizationState = "Complete"
    /\ activationCommitted
    /\ CurrentRealizationWork(realizationOwner)

CreateWithInitialEvent ==
    /\ Root!Create(SessionOwner, Payload, "Worker", TRUE)
    /\ Event!AcceptInitialEvent
    /\ ~workRowPresent
    /\ UNCHANGED <<workRowPresent, workVars, realizationWorkEpoch>>

ReplayCreate ==
    /\ Root!ExactCreateReplay(SessionOwner, Payload)
    /\ UNCHANGED <<eventVars, workRowPresent, workVars, realizationWorkEpoch>>

ProjectWork ==
    /\ Root!ProjectWork
    /\ ~workRowPresent
    /\ workRowPresent' = TRUE
    /\ UNCHANGED <<eventVars, workVars, realizationWorkEpoch>>

LoseQueuedWorkProjection ==
    /\ workRowPresent
    /\ workState[WorkItem] = "Queued"
    /\ Root!LoseWorkProjection
    /\ workRowPresent' = FALSE
    /\ UNCHANGED <<eventVars, workVars, realizationWorkEpoch>>

ClaimWork(worker) ==
    /\ workRowPresent
    /\ WorkKernel!Claim(worker, WorkItem)
    /\ UNCHANGED <<rootVars, eventVars, workRowPresent,
                   realizationWorkEpoch>>

ReclaimWork(worker) ==
    /\ workRowPresent
    /\ WorkKernel!Reclaim(worker, WorkItem)
    /\ UNCHANGED <<rootVars, eventVars, workRowPresent,
                   realizationWorkEpoch>>

HeartbeatWork(worker, expected) ==
    /\ workRowPresent
    /\ WorkKernel!Heartbeat(worker, WorkItem, expected)
    /\ UNCHANGED <<rootVars, eventVars, workRowPresent,
                   realizationWorkEpoch>>

AdvanceStoreTime ==
    /\ WorkKernel!AdvanceTime
    /\ UNCHANGED <<rootVars, eventVars, workRowPresent,
                   realizationWorkEpoch>>

BeginRealization(worker) ==
    /\ CurrentWork(worker)
    /\ Root!BeginRealization(worker)
    /\ realizationWorkEpoch' = workEpoch[WorkItem]
    /\ UNCHANGED <<eventVars, workRowPresent, workVars>>

StageRealization(worker, epoch) ==
    /\ CurrentRealizationWork(worker)
    /\ Root!StageRealization(worker, epoch)
    /\ UNCHANGED <<eventVars, workRowPresent, workVars,
                   realizationWorkEpoch>>

ActivateRealization(worker, epoch) ==
    /\ CurrentRealizationWork(worker)
    /\ Root!ActivateRealization(worker, epoch)
    /\ UNCHANGED <<eventVars, workRowPresent, workVars,
                   realizationWorkEpoch>>

CompleteRealization(worker, epoch) ==
    /\ CurrentRealizationWork(worker)
    /\ Root!CompleteRealization(worker, epoch,
         activeActivityEpochs # {})
    /\ UNCHANGED <<eventVars, workRowPresent, workVars,
                   realizationWorkEpoch>>

\* A replacement Work epoch invalidates every remaining physical effect of the
\* predecessor. Recovery may classify the old durable realization as retryable
\* without granting the recovery actor the predecessor's external authority.
FenceStaleRealization ==
    /\ realizationState \in {"Leased", "Staged", "Activated", "Complete"}
    /\ execution # "Idle"
    /\ \/ ~workRowPresent
       \/ workState[WorkItem] # "Active"
       \/ workOwner[WorkItem] # realizationOwner
       \/ workEpoch[WorkItem] # realizationWorkEpoch
    /\ Root!RetryableRealizationFailure(realizationOwner, realizationEpoch)
    /\ realizationWorkEpoch' = 0
    /\ UNCHANGED <<eventVars, workRowPresent, workVars>>

ReserveRun(worker) ==
    /\ SessionExecutable
    /\ CurrentWork(worker)
    /\ Event!ReserveRun
    /\ UNCHANGED <<rootVars, workRowPresent, workVars,
                   realizationWorkEpoch>>

RegisterRunActivity(worker) ==
    /\ SessionExecutable
    /\ CurrentWork(worker)
    /\ Event!RegisterRunActivity
    /\ UNCHANGED <<rootVars, workRowPresent, workVars,
                   realizationWorkEpoch>>

ActivateRun(worker) ==
    /\ SessionExecutable
    /\ CurrentWork(worker)
    /\ Event!ActivateRun
    /\ UNCHANGED <<rootVars, workRowPresent, workVars,
                   realizationWorkEpoch>>

ClaimRun(worker) ==
    /\ SessionExecutable
    /\ CurrentWork(worker)
    /\ Event!ClaimRun(worker)
    /\ UNCHANGED <<rootVars, workRowPresent, workVars,
                   realizationWorkEpoch>>

ObserveCommittedRun(worker, epoch, disposition) ==
    /\ SessionExecutable
    /\ CurrentWork(worker)
    /\ Event!ObserveCommittedRun(worker, epoch, disposition)
    /\ UNCHANGED <<rootVars, workRowPresent, workVars,
                   realizationWorkEpoch>>

SettleRunActivity ==
    /\ Event!SettleRunActivity
    /\ UNCHANGED <<rootVars, workRowPresent, workVars,
                   realizationWorkEpoch>>

SettleDispatch(worker, epoch) ==
    /\ Event!SettleCommittedRun(worker, epoch)
    /\ UNCHANGED <<rootVars, workRowPresent, workVars,
                   realizationWorkEpoch>>

MarkEventProcessed ==
    /\ Event!MarkProcessed
    /\ UNCHANGED <<rootVars, workRowPresent, workVars,
                   realizationWorkEpoch>>

SettleEventAndClose ==
    /\ activeActivityEpochs = {activityEpoch[EventOperation]}
    /\ Event!SettleInitialEventWake
    /\ Root!CloseRunningInterval
    /\ UNCHANGED <<workRowPresent, workVars, realizationWorkEpoch>>

ReleaseWork(worker, epoch) ==
    /\ execution = "Idle"
    /\ eventPhase = "Processed"
    /\ activeActivityEpochs = {}
    /\ WorkKernel!Stop(worker, WorkItem, epoch)
    /\ UNCHANGED <<rootVars, eventVars, workRowPresent,
                   realizationWorkEpoch>>

TerminateSession ==
    /\ Root!Terminate
    /\ UNCHANGED <<eventVars, workRowPresent, workVars,
                   realizationWorkEpoch>>

Next ==
    \/ CreateWithInitialEvent
    \/ ReplayCreate
    \/ ProjectWork
    \/ LoseQueuedWorkProjection
    \/ \E worker \in Workers: ClaimWork(worker)
    \/ \E worker \in Workers: ReclaimWork(worker)
    \/ \E worker \in Workers, expected \in 0..MaxHeartbeat:
         HeartbeatWork(worker, expected)
    \/ AdvanceStoreTime
    \/ \E worker \in Workers: BeginRealization(worker)
    \/ \E worker \in Workers, epoch \in 0..MaxEpoch:
         StageRealization(worker, epoch)
    \/ \E worker \in Workers, epoch \in 0..MaxEpoch:
         ActivateRealization(worker, epoch)
    \/ \E worker \in Workers, epoch \in 0..MaxEpoch:
         CompleteRealization(worker, epoch)
    \/ FenceStaleRealization
    \/ \E worker \in Workers: ReserveRun(worker)
    \/ \E worker \in Workers: RegisterRunActivity(worker)
    \/ \E worker \in Workers: ActivateRun(worker)
    \/ \E worker \in Workers: ClaimRun(worker)
    \/ \E worker \in Workers, epoch \in 0..MaxEpoch,
          disposition \in {"Awaiting", "Ended"}:
         ObserveCommittedRun(worker, epoch, disposition)
    \/ SettleRunActivity
    \/ \E worker \in Workers, epoch \in 0..MaxEpoch:
         SettleDispatch(worker, epoch)
    \/ MarkEventProcessed
    \/ SettleEventAndClose
    \/ \E worker \in Workers, epoch \in 0..MaxEpoch:
         ReleaseWork(worker, epoch)
    \/ TerminateSession

Spec == Init /\ [][Next]_vars

\* Fault-free witness relation. Fault/replay actions remain in Spec for safety,
\* while this smaller relation proves the intended create-to-release path is
\* reachable under weak fairness without claiming liveness under endless loss.
HappyNext ==
    \/ CreateWithInitialEvent
    \/ ProjectWork
    \/ \E worker \in Workers: ClaimWork(worker)
    \/ \E worker \in Workers: BeginRealization(worker)
    \/ \E worker \in Workers, epoch \in 0..MaxEpoch:
         StageRealization(worker, epoch)
    \/ \E worker \in Workers, epoch \in 0..MaxEpoch:
         ActivateRealization(worker, epoch)
    \/ \E worker \in Workers, epoch \in 0..MaxEpoch:
         CompleteRealization(worker, epoch)
    \/ \E worker \in Workers: ReserveRun(worker)
    \/ \E worker \in Workers: RegisterRunActivity(worker)
    \/ \E worker \in Workers: ActivateRun(worker)
    \/ \E worker \in Workers: ClaimRun(worker)
    \/ \E worker \in Workers, epoch \in 0..MaxEpoch,
          disposition \in {"Awaiting", "Ended"}:
         ObserveCommittedRun(worker, epoch, disposition)
    \/ SettleRunActivity
    \/ \E worker \in Workers, epoch \in 0..MaxEpoch:
         SettleDispatch(worker, epoch)
    \/ MarkEventProcessed
    \/ SettleEventAndClose
    \/ \E worker \in Workers, epoch \in 0..MaxEpoch:
         ReleaseWork(worker, epoch)

FullySettled ==
    /\ existence = "Live"
    /\ execution = "Idle"
    /\ eventPhase = "Processed"
    /\ settledActivities = {EventOperation, RunOperation}
    /\ workState[WorkItem] = "Stopped"
    /\ dispatchState \in {"Awaiting", "Removed"}

HappySpec == Init /\ [][HappyNext]_vars /\ WF_vars(HappyNext)
EventuallyFullySettled == <>FullySettled

TypeOK ==
    /\ Root!TypeOK
    /\ Event!TypeOK
    /\ WorkKernel!TypeOK
    /\ workRowPresent \in BOOLEAN
    /\ realizationWorkEpoch \in 0..MaxEpoch

CompleteRootPrecedesEveryProjection ==
    (workRowPresent \/ dispatchExists) =>
        /\ existence = "Live"
        /\ baseline = "Frozen"
        /\ createReceipt

WorkProjectionHasOneAuthority == workRowPresent = workProjected

IdleReleaseRequiresFullSettlement ==
    workState[WorkItem] = "Stopped" =>
        /\ execution \in {"Idle", "Terminated"}
        /\ eventPhase = "Processed"
        /\ activeActivityEpochs = {}

Safety ==
    /\ TypeOK
    /\ Root!Safety
    /\ Event!Safety
    /\ WorkKernel!Safety
    /\ CompleteRootPrecedesEveryProjection
    /\ WorkProjectionHasOneAuthority
    /\ IdleReleaseRequiresFullSettlement

=============================================================================
