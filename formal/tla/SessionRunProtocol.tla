------------------------- MODULE SessionRunProtocol -------------------------
EXTENDS Naturals

\* Multi-Run composition. Dispatch, Session activity and WorkQueue transitions
\* are instantiated from their canonical kernels. This module owns only the
\* cross-domain authority predicates and the ordering of durable observations.
CONSTANTS
    RootRun, ChildRunA, ChildRunB,
    RootThread, ChildThread,
    WorkItem, Workers, NoWorker, MaxEpoch, MaxHeartbeat, MaxTime

Runs == {RootRun, ChildRunA, ChildRunB}
RootRuns == {RootRun}
WorkItems == {WorkItem}
RunThread(run) == IF run = RootRun THEN RootThread ELSE ChildThread

VARIABLES
    rootState, rootOwner, rootLeaseEpoch, rootCancel, rootInput,
    childAState, childAOwner, childALeaseEpoch, childACancel, childAInput,
    childBState, childBOwner, childBLeaseEpoch, childBCancel, childBInput,
    activityEpoch, activeActivityEpochs, settledActivities, nextActivityEpoch,
    workState, workOwner, workEpoch, workHeartbeat, workExpires, workNow,
    realizedOwner, realizedWorkEpoch, releasedWorkEpoch,
    executionStarted, executionOwner, executionEpoch,
    committedDisposition, committedOwner, committedEpoch, committedWorkEpoch,
    sessionObservationApplied

rootVars == <<rootState, rootOwner, rootLeaseEpoch, rootCancel, rootInput>>
childAVars == <<childAState, childAOwner, childALeaseEpoch, childACancel, childAInput>>
childBVars == <<childBState, childBOwner, childBLeaseEpoch, childBCancel, childBInput>>
dispatchVars == <<rootVars, childAVars, childBVars>>
activityVars == <<activityEpoch, activeActivityEpochs, settledActivities,
                  nextActivityEpoch>>
workVars == <<workState, workOwner, workEpoch, workHeartbeat, workExpires,
              workNow>>
protocolVars == <<activityVars, workVars, realizedOwner, realizedWorkEpoch,
                  releasedWorkEpoch, executionStarted, executionOwner,
                  executionEpoch, committedDisposition,
                  committedOwner, committedEpoch, committedWorkEpoch,
                  sessionObservationApplied>>
vars == <<dispatchVars, protocolVars>>

RootIngress == INSTANCE RunIngressKernel WITH
    Owners <- Workers, NoOwner <- NoWorker, MaxEpoch <- MaxEpoch,
    kState <- rootState, kOwner <- rootOwner,
    kLeaseEpoch <- rootLeaseEpoch, kCancelRequested <- rootCancel,
    kPendingInput <- rootInput

ChildAIngress == INSTANCE RunIngressKernel WITH
    Owners <- Workers, NoOwner <- NoWorker, MaxEpoch <- MaxEpoch,
    kState <- childAState, kOwner <- childAOwner,
    kLeaseEpoch <- childALeaseEpoch, kCancelRequested <- childACancel,
    kPendingInput <- childAInput

ChildBIngress == INSTANCE RunIngressKernel WITH
    Owners <- Workers, NoOwner <- NoWorker, MaxEpoch <- MaxEpoch,
    kState <- childBState, kOwner <- childBOwner,
    kLeaseEpoch <- childBLeaseEpoch, kCancelRequested <- childBCancel,
    kPendingInput <- childBInput

ActivityKernel == INSTANCE SessionActivityKernel WITH
    ActivityIds <- Runs,
    MaxActivityEpoch <- MaxEpoch,
    kActivityEpoch <- activityEpoch,
    kActiveActivityEpochs <- activeActivityEpochs,
    kSettledActivities <- settledActivities,
    kNextActivityEpoch <- nextActivityEpoch

WorkKernel == INSTANCE WorkQueueKernel WITH
    WorkItems <- WorkItems,
    Workers <- Workers,
    NoWorker <- NoWorker,
    MaxEpoch <- MaxEpoch,
    MaxHeartbeat <- MaxHeartbeat,
    MaxTime <- MaxTime,
    kState <- workState,
    kOwner <- workOwner,
    kEpoch <- workEpoch,
    kHeartbeat <- workHeartbeat,
    kExpires <- workExpires,
    kNow <- workNow

DState(run) == CASE run = RootRun -> rootState
                     [] run = ChildRunA -> childAState
                     [] OTHER -> childBState
DOwner(run) == CASE run = RootRun -> rootOwner
                     [] run = ChildRunA -> childAOwner
                     [] OTHER -> childBOwner
DEpoch(run) == CASE run = RootRun -> rootLeaseEpoch
                     [] run = ChildRunA -> childALeaseEpoch
                     [] OTHER -> childBLeaseEpoch

CurrentWork(candidate) ==
    /\ workState[WorkItem] = "Active"
    /\ workOwner[WorkItem] = candidate

CurrentRealization(candidate) ==
    /\ CurrentWork(candidate)
    /\ realizedOwner = candidate
    /\ realizedWorkEpoch = workEpoch[WorkItem]

AuthoritativeAttempt(run) ==
    /\ run \in executionStarted
    /\ DState(run) = "Leased"
    /\ executionOwner[run] = DOwner(run)
    /\ executionEpoch[run] = DEpoch(run)
    /\ CurrentRealization(executionOwner[run])

Init ==
    /\ RootIngress!Init
    /\ ChildAIngress!Init
    /\ ChildBIngress!Init
    /\ ActivityKernel!Init
    /\ WorkKernel!Init
    /\ realizedOwner = NoWorker
    /\ realizedWorkEpoch = 0
    /\ releasedWorkEpoch = 0
    /\ executionStarted = {}
    /\ executionOwner = [run \in Runs |-> NoWorker]
    /\ executionEpoch = [run \in Runs |-> 0]
    /\ committedDisposition = [run \in Runs |-> "None"]
    /\ committedOwner = [run \in Runs |-> NoWorker]
    /\ committedEpoch = [run \in Runs |-> 0]
    /\ committedWorkEpoch = [run \in Runs |-> 0]
    /\ sessionObservationApplied = {}

ActivateDispatch(run) ==
    \/ /\ run = RootRun
       /\ RootIngress!ActivateReservation
       /\ UNCHANGED <<childAVars, childBVars>>
    \/ /\ run = ChildRunA
       /\ ChildAIngress!ActivateReservation
       /\ UNCHANGED <<rootVars, childBVars>>
    \/ /\ run = ChildRunB
       /\ ChildBIngress!ActivateReservation
       /\ UNCHANGED <<rootVars, childAVars>>

ClaimDispatch(run, candidate) ==
    \/ /\ run = RootRun
       /\ RootIngress!Claim(candidate)
       /\ UNCHANGED <<childAVars, childBVars>>
    \/ /\ run = ChildRunA
       /\ ChildAIngress!Claim(candidate)
       /\ UNCHANGED <<rootVars, childBVars>>
    \/ /\ run = ChildRunB
       /\ ChildBIngress!Claim(candidate)
       /\ UNCHANGED <<rootVars, childAVars>>

SettleAwaitingDispatch(run, candidate, epoch) ==
    \/ /\ run = RootRun
       /\ RootIngress!SettleAwaiting(candidate, epoch)
       /\ UNCHANGED <<childAVars, childBVars>>
    \/ /\ run = ChildRunA
       /\ ChildAIngress!SettleAwaiting(candidate, epoch)
       /\ UNCHANGED <<rootVars, childBVars>>
    \/ /\ run = ChildRunB
       /\ ChildBIngress!SettleAwaiting(candidate, epoch)
       /\ UNCHANGED <<rootVars, childAVars>>

SettleDoneDispatch(run, candidate, epoch) ==
    \/ /\ run = RootRun
       /\ RootIngress!SettleDone(candidate, epoch)
       /\ UNCHANGED <<childAVars, childBVars>>
    \/ /\ run = ChildRunA
       /\ ChildAIngress!SettleDone(candidate, epoch)
       /\ UNCHANGED <<rootVars, childBVars>>
    \/ /\ run = ChildRunB
       /\ ChildBIngress!SettleDone(candidate, epoch)
       /\ UNCHANGED <<rootVars, childAVars>>

OpenActivity(run) ==
    /\ DState(run) = "Reserved"
    /\ ActivityKernel!Open(run)
    /\ UNCHANGED <<dispatchVars, workVars, realizedOwner,
                   realizedWorkEpoch, releasedWorkEpoch, executionStarted,
                   executionOwner, executionEpoch,
                   committedDisposition, committedOwner, committedEpoch,
                   committedWorkEpoch, sessionObservationApplied>>

ActivateReservation(run) ==
    /\ activityEpoch[run] > 0
    /\ ActivateDispatch(run)
    /\ UNCHANGED protocolVars

ClaimWork(candidate) ==
    /\ WorkKernel!Claim(candidate, WorkItem)
    /\ UNCHANGED <<dispatchVars, activityVars, realizedOwner,
                   realizedWorkEpoch, releasedWorkEpoch, executionStarted,
                   executionOwner, executionEpoch,
                   committedDisposition, committedOwner, committedEpoch,
                   committedWorkEpoch, sessionObservationApplied>>

ReclaimWork(candidate) ==
    /\ WorkKernel!Reclaim(candidate, WorkItem)
    /\ UNCHANGED <<dispatchVars, activityVars, realizedOwner,
                   realizedWorkEpoch, releasedWorkEpoch, executionStarted,
                   executionOwner, executionEpoch,
                   committedDisposition, committedOwner, committedEpoch,
                   committedWorkEpoch, sessionObservationApplied>>

HeartbeatWork(candidate, expected) ==
    /\ WorkKernel!Heartbeat(candidate, WorkItem, expected)
    /\ UNCHANGED <<dispatchVars, activityVars, realizedOwner,
                   realizedWorkEpoch, releasedWorkEpoch, executionStarted,
                   executionOwner, executionEpoch,
                   committedDisposition, committedOwner, committedEpoch,
                   committedWorkEpoch, sessionObservationApplied>>

AdvanceStoreTime ==
    /\ WorkKernel!AdvanceTime
    /\ UNCHANGED <<dispatchVars, activityVars, realizedOwner,
                   realizedWorkEpoch, releasedWorkEpoch, executionStarted,
                   executionOwner, executionEpoch,
                   committedDisposition, committedOwner, committedEpoch,
                   committedWorkEpoch, sessionObservationApplied>>

Realize(candidate) ==
    /\ CurrentWork(candidate)
    /\ realizedOwner' = candidate
    /\ realizedWorkEpoch' = workEpoch[WorkItem]
    /\ UNCHANGED <<dispatchVars, activityVars, workVars,
                   releasedWorkEpoch, executionStarted, executionOwner,
                   executionEpoch,
                   committedDisposition, committedOwner, committedEpoch,
                   committedWorkEpoch, sessionObservationApplied>>

ThreadAvailable(run) ==
    \A peer \in Runs \ {run}:
        RunThread(peer) = RunThread(run) =>
            /\ ~AuthoritativeAttempt(peer)
            /\ DState(peer) \notin {"ReservationLeased", "Leased", "Awaiting"}

ClaimRun(run, candidate) ==
    /\ committedDisposition[run] = "None"
    /\ ThreadAvailable(run)
    /\ CurrentRealization(candidate)
    /\ DState(run) \in {"Pending", "Awaiting"}
    /\ ClaimDispatch(run, candidate)
    /\ UNCHANGED <<activityVars, workVars, realizedOwner,
                   realizedWorkEpoch, releasedWorkEpoch,
                   executionStarted, executionOwner, executionEpoch,
                   committedDisposition, committedOwner, committedEpoch,
                   committedWorkEpoch, sessionObservationApplied>>

ReclaimRun(run, candidate) ==
    /\ CurrentRealization(candidate)
    /\ DState(run) = "Leased"
    /\ ClaimDispatch(run, candidate)
    /\ UNCHANGED <<activityVars, workVars, realizedOwner,
                   realizedWorkEpoch, releasedWorkEpoch,
                   executionStarted, executionOwner, executionEpoch,
                   committedDisposition, committedOwner, committedEpoch,
                   committedWorkEpoch, sessionObservationApplied>>

StartExecution(run, candidate, epoch) ==
    /\ run \notin executionStarted
    /\ DState(run) = "Leased"
    /\ DOwner(run) = candidate
    /\ DEpoch(run) = epoch
    /\ CurrentRealization(candidate)
    /\ \A peer \in Runs \ {run}:
         RunThread(peer) = RunThread(run) => peer \notin executionStarted
    /\ executionStarted' = executionStarted \cup {run}
    /\ executionOwner' = [executionOwner EXCEPT ![run] = candidate]
    /\ executionEpoch' = [executionEpoch EXCEPT ![run] = epoch]
    /\ UNCHANGED <<dispatchVars, activityVars, workVars, realizedOwner,
                   realizedWorkEpoch, releasedWorkEpoch,
                   committedDisposition, committedOwner, committedEpoch,
                   committedWorkEpoch, sessionObservationApplied>>

FinishExecution(run, candidate, epoch) ==
    /\ run \in executionStarted
    /\ executionOwner[run] = candidate
    /\ executionEpoch[run] = epoch
    /\ executionStarted' = executionStarted \ {run}
    /\ executionOwner' = [executionOwner EXCEPT ![run] = NoWorker]
    /\ executionEpoch' = [executionEpoch EXCEPT ![run] = 0]
    /\ UNCHANGED <<dispatchVars, activityVars, workVars, realizedOwner,
                   realizedWorkEpoch, releasedWorkEpoch,
                   committedDisposition, committedOwner, committedEpoch,
                   committedWorkEpoch, sessionObservationApplied>>

\* ThreadCommit remains the sole Run-disposition writer. This action records
\* only the committed fact visible under the exact Dispatch and Work fences.
ObserveCommittedRun(run, disposition, candidate, epoch) ==
    /\ disposition \in {"Awaiting", "Ended"}
    /\ committedDisposition[run] = "None"
    /\ AuthoritativeAttempt(run)
    /\ DOwner(run) = candidate
    /\ DEpoch(run) = epoch
    /\ CurrentRealization(candidate)
    /\ committedDisposition' = [committedDisposition EXCEPT ![run] = disposition]
    /\ committedOwner' = [committedOwner EXCEPT ![run] = candidate]
    /\ committedEpoch' = [committedEpoch EXCEPT ![run] = epoch]
    /\ committedWorkEpoch' =
         [committedWorkEpoch EXCEPT ![run] = workEpoch[WorkItem]]
    /\ UNCHANGED <<dispatchVars, activityVars, workVars, realizedOwner,
                   realizedWorkEpoch, releasedWorkEpoch,
                   executionStarted, executionOwner, executionEpoch,
                   sessionObservationApplied>>

ApplySessionObservation(run) ==
    /\ committedDisposition[run] \in {"Awaiting", "Ended"}
    /\ run \notin sessionObservationApplied
    /\ ActivityKernel!Settle(run)
    /\ sessionObservationApplied' = sessionObservationApplied \cup {run}
    /\ UNCHANGED <<dispatchVars, workVars, realizedOwner,
                   realizedWorkEpoch, releasedWorkEpoch, executionStarted,
                   executionOwner, executionEpoch,
                   committedDisposition, committedOwner, committedEpoch,
                   committedWorkEpoch>>

\* Root settlement consumes the complete Work lease receipt before Dispatch
\* removal. A same-owner higher-epoch successor therefore rejects stale stop.
ReleaseWork(run, candidate, epoch) ==
    /\ run \in RootRuns
    /\ run \in sessionObservationApplied
    /\ \A attempt \in Runs: attempt \notin executionStarted
    /\ WorkKernel!Stop(candidate, WorkItem, epoch)
    /\ releasedWorkEpoch' = epoch
    /\ realizedOwner' = NoWorker
    /\ realizedWorkEpoch' = 0
    /\ UNCHANGED <<dispatchVars, activityVars, executionStarted,
                   executionOwner, executionEpoch,
                   committedDisposition, committedOwner, committedEpoch,
                   committedWorkEpoch, sessionObservationApplied>>

RootSettlementReady(run) ==
    (run \notin RootRuns) \/ workState[WorkItem] = "Stopped"

SettleAwaiting(run, candidate, epoch) ==
    /\ committedDisposition[run] = "Awaiting"
    /\ run \in sessionObservationApplied
    /\ RootSettlementReady(run)
    /\ run \notin executionStarted
    /\ SettleAwaitingDispatch(run, candidate, epoch)
    /\ UNCHANGED <<activityVars, workVars, realizedOwner,
                   realizedWorkEpoch, releasedWorkEpoch,
                   executionStarted, executionOwner, executionEpoch,
                   committedDisposition, committedOwner, committedEpoch,
                   committedWorkEpoch, sessionObservationApplied>>

SettleDone(run, candidate, epoch) ==
    /\ committedDisposition[run] = "Ended"
    /\ run \in sessionObservationApplied
    /\ RootSettlementReady(run)
    /\ run \notin executionStarted
    /\ SettleDoneDispatch(run, candidate, epoch)
    /\ UNCHANGED <<activityVars, workVars, realizedOwner,
                   realizedWorkEpoch, releasedWorkEpoch,
                   executionStarted, executionOwner, executionEpoch,
                   committedDisposition, committedOwner, committedEpoch,
                   committedWorkEpoch, sessionObservationApplied>>

Next ==
    \/ \E run \in Runs: OpenActivity(run)
    \/ \E run \in Runs: ActivateReservation(run)
    \/ \E worker \in Workers: ClaimWork(worker)
    \/ \E worker \in Workers: ReclaimWork(worker)
    \/ \E worker \in Workers, expected \in 0..MaxHeartbeat:
         HeartbeatWork(worker, expected)
    \/ AdvanceStoreTime
    \/ \E worker \in Workers: Realize(worker)
    \/ \E run \in Runs, worker \in Workers: ClaimRun(run, worker)
    \/ \E run \in Runs, worker \in Workers: ReclaimRun(run, worker)
    \/ \E run \in Runs, worker \in Workers, epoch \in 0..MaxEpoch:
         StartExecution(run, worker, epoch)
    \/ \E run \in Runs, worker \in Workers, epoch \in 0..MaxEpoch:
         FinishExecution(run, worker, epoch)
    \/ \E run \in Runs, disposition \in {"Awaiting", "Ended"},
          worker \in Workers, epoch \in 0..MaxEpoch:
         ObserveCommittedRun(run, disposition, worker, epoch)
    \/ \E run \in Runs: ApplySessionObservation(run)
    \/ \E run \in Runs, worker \in Workers, epoch \in 0..MaxEpoch:
         ReleaseWork(run, worker, epoch)
    \/ \E run \in Runs, worker \in Workers, epoch \in 0..MaxEpoch:
         SettleAwaiting(run, worker, epoch)
    \/ \E run \in Runs, worker \in Workers, epoch \in 0..MaxEpoch:
         SettleDone(run, worker, epoch)

Spec == Init /\ [][Next]_vars

TypeOK ==
    /\ RootRun # ChildRunA
    /\ RootRun # ChildRunB
    /\ ChildRunA # ChildRunB
    /\ RootThread # ChildThread
    /\ realizedOwner \in Workers \cup {NoWorker}
    /\ realizedWorkEpoch \in 0..MaxEpoch
    /\ releasedWorkEpoch \in 0..MaxEpoch
    /\ executionStarted \subseteq Runs
    /\ executionOwner \in [Runs -> (Workers \cup {NoWorker})]
    /\ executionEpoch \in [Runs -> 0..MaxEpoch]
    /\ \A run \in Runs:
         (run \in executionStarted) =
            (executionOwner[run] \in Workers /\ executionEpoch[run] > 0)
    /\ committedDisposition \in [Runs -> {"None", "Awaiting", "Ended"}]
    /\ committedOwner \in [Runs -> (Workers \cup {NoWorker})]
    /\ committedEpoch \in [Runs -> 0..MaxEpoch]
    /\ committedWorkEpoch \in [Runs -> 0..MaxEpoch]
    /\ sessionObservationApplied \subseteq Runs

IngressSafety ==
    /\ RootIngress!Safety
    /\ ChildAIngress!Safety
    /\ ChildBIngress!Safety

ActivityBeforeExecutableDispatch ==
    \A run \in Runs:
        DState(run) \in {"Pending", "Leased", "Awaiting", "Removed"} =>
            activityEpoch[run] > 0

ReservationCannotExecute ==
    \A run \in Runs:
        DState(run) \in {"Reserved", "ReservationLeased"} =>
            ~AuthoritativeAttempt(run)

OneExecutionAuthorityPerThread ==
    \A left, right \in Runs:
        AuthoritativeAttempt(left) /\ AuthoritativeAttempt(right) /\
        RunThread(left) = RunThread(right) => left = right

OnePhysicalAttemptPerThread ==
    \A left, right \in executionStarted:
        RunThread(left) = RunThread(right) => left = right

CommittedObservationHasExactReceipts ==
    \A run \in Runs:
        committedDisposition[run] # "None" =>
            /\ committedEpoch[run] > 0
            /\ committedWorkEpoch[run] > 0
            /\ committedOwner[run] \in Workers
            /\ committedWorkEpoch[run] <= workEpoch[WorkItem]

ObservationRequiresCommit ==
    \A run \in sessionObservationApplied:
        committedDisposition[run] # "None"

SettlementRequiresObservation ==
    \A run \in Runs:
        DState(run) \in {"Awaiting", "Removed"} =>
            run \in sessionObservationApplied

ReleasedWorkIsEpochFenced == workState[WorkItem] = "Stopped" =>
    /\ workOwner[WorkItem] = NoWorker
    /\ releasedWorkEpoch = workEpoch[WorkItem]
    /\ releasedWorkEpoch > 0
    /\ \A run \in Runs: ~AuthoritativeAttempt(run)

Safety ==
    /\ TypeOK
    /\ IngressSafety
    /\ ActivityKernel!Safety
    /\ WorkKernel!Safety
    /\ ActivityBeforeExecutableDispatch
    /\ ReservationCannotExecute
    /\ OneExecutionAuthorityPerThread
    /\ OnePhysicalAttemptPerThread
    /\ CommittedObservationHasExactReceipts
    /\ ObservationRequiresCommit
    /\ SettlementRequiresObservation
    /\ ReleasedWorkIsEpochFenced

=============================================================================
