------------------------- MODULE SessionRunProtocol -------------------------
EXTENDS Naturals

\* Three bounded Runs are sufficient to exercise both required concurrency
\* classes: root and child A have distinct Threads and may overlap; child A and
\* child B share one Thread and must never overlap. Each row is a direct
\* RunIngressKernel instance, so this composition adds no Dispatch transitions.
CONSTANTS
    RootRun, ChildRunA, ChildRunB,
    RootThread, ChildThread,
    Workers, NoWorker, MaxEpoch

Runs == {RootRun, ChildRunA, ChildRunB}
RootRuns == {RootRun}
RunThread(run) == IF run = RootRun THEN RootThread ELSE ChildThread

VARIABLES
    rootState, rootOwner, rootLeaseEpoch, rootCancel, rootInput,
    childAState, childAOwner, childALeaseEpoch, childACancel, childAInput,
    childBState, childBOwner, childBLeaseEpoch, childBCancel, childBInput,
    activityEpoch, activeActivityEpochs, nextActivityEpoch,
    workState, workOwner, workEpoch, releasedWorkEpoch,
    realizationReady, executionStarted,
    committedDisposition, committedOwner, committedEpoch,
    sessionObservationApplied

rootVars == <<rootState, rootOwner, rootLeaseEpoch, rootCancel, rootInput>>
childAVars == <<childAState, childAOwner, childALeaseEpoch, childACancel, childAInput>>
childBVars == <<childBState, childBOwner, childBLeaseEpoch, childBCancel, childBInput>>
dispatchVars == <<rootVars, childAVars, childBVars>>
protocolVars == <<activityEpoch, activeActivityEpochs, nextActivityEpoch,
                  workState, workOwner, workEpoch, releasedWorkEpoch,
                  realizationReady, executionStarted,
                  committedDisposition, committedOwner, committedEpoch,
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

DState(run) == CASE run = RootRun -> rootState
                     [] run = ChildRunA -> childAState
                     [] OTHER -> childBState
DOwner(run) == CASE run = RootRun -> rootOwner
                     [] run = ChildRunA -> childAOwner
                     [] OTHER -> childBOwner
DEpoch(run) == CASE run = RootRun -> rootLeaseEpoch
                     [] run = ChildRunA -> childALeaseEpoch
                     [] OTHER -> childBLeaseEpoch

Init ==
    /\ RootIngress!Init
    /\ ChildAIngress!Init
    /\ ChildBIngress!Init
    /\ activityEpoch = [run \in Runs |-> 0]
    /\ activeActivityEpochs = {}
    /\ nextActivityEpoch = 1
    /\ workState = "Queued"
    /\ workOwner = NoWorker
    /\ workEpoch = 0
    /\ releasedWorkEpoch = 0
    /\ realizationReady = FALSE
    /\ executionStarted = {}
    /\ committedDisposition = [run \in Runs |-> "None"]
    /\ committedOwner = [run \in Runs |-> NoWorker]
    /\ committedEpoch = [run \in Runs |-> 0]
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
    /\ run \in Runs
    /\ DState(run) = "Reserved"
    /\ activityEpoch[run] = 0
    /\ nextActivityEpoch <= MaxEpoch
    /\ activityEpoch' = [activityEpoch EXCEPT ![run] = nextActivityEpoch]
    /\ activeActivityEpochs' = activeActivityEpochs \cup {nextActivityEpoch}
    /\ nextActivityEpoch' = nextActivityEpoch + 1
    /\ UNCHANGED <<dispatchVars, workState, workOwner, workEpoch,
                   releasedWorkEpoch, realizationReady, executionStarted,
                   committedDisposition, committedOwner, committedEpoch,
                   sessionObservationApplied>>

ActivateReservation(run) ==
    /\ activityEpoch[run] > 0
    /\ ActivateDispatch(run)
    /\ UNCHANGED protocolVars

ClaimWork(candidate) ==
    /\ candidate \in Workers
    /\ workState = "Queued"
    /\ workEpoch < MaxEpoch
    /\ workState' = "Leased"
    /\ workOwner' = candidate
    /\ workEpoch' = workEpoch + 1
    /\ UNCHANGED <<dispatchVars, activityEpoch, activeActivityEpochs,
                   nextActivityEpoch, releasedWorkEpoch, realizationReady,
                   executionStarted, committedDisposition, committedOwner,
                   committedEpoch, sessionObservationApplied>>

ExpireWork ==
    /\ workState = "Leased"
    /\ executionStarted = {}
    /\ workState' = "Queued"
    /\ workOwner' = NoWorker
    /\ realizationReady' = FALSE
    /\ UNCHANGED <<dispatchVars, activityEpoch, activeActivityEpochs,
                   nextActivityEpoch, workEpoch, releasedWorkEpoch,
                   executionStarted, committedDisposition, committedOwner,
                   committedEpoch, sessionObservationApplied>>

Realize(candidate, epoch) ==
    /\ workState = "Leased"
    /\ workOwner = candidate
    /\ workEpoch = epoch
    /\ realizationReady' = TRUE
    /\ UNCHANGED <<dispatchVars, activityEpoch, activeActivityEpochs,
                   nextActivityEpoch, workState, workOwner, workEpoch,
                   releasedWorkEpoch, executionStarted, committedDisposition,
                   committedOwner, committedEpoch, sessionObservationApplied>>

ThreadAvailable(run) ==
    \A peer \in Runs \ {run}:
        RunThread(peer) = RunThread(run) =>
            /\ peer \notin executionStarted
            /\ DState(peer) \notin {"ReservationLeased", "Leased", "Awaiting"}

ClaimRun(run, candidate) ==
    /\ committedDisposition[run] = "None"
    /\ ThreadAvailable(run)
    /\ workState = "Leased"
    /\ workOwner = candidate
    /\ realizationReady
    /\ DState(run) \in {"Pending", "Awaiting"}
    /\ ClaimDispatch(run, candidate)
    /\ executionStarted' = executionStarted \cup {run}
    /\ UNCHANGED <<activityEpoch, activeActivityEpochs, nextActivityEpoch,
                   workState, workOwner, workEpoch, releasedWorkEpoch,
                   realizationReady, committedDisposition, committedOwner,
                   committedEpoch, sessionObservationApplied>>

ReclaimRun(run, candidate) ==
    /\ workState = "Leased"
    /\ workOwner = candidate
    /\ realizationReady
    /\ DState(run) = "Leased"
    /\ ClaimDispatch(run, candidate)
    /\ executionStarted' = IF committedDisposition[run] = "None"
                              THEN executionStarted \cup {run}
                              ELSE executionStarted \ {run}
    /\ UNCHANGED <<activityEpoch, activeActivityEpochs, nextActivityEpoch,
                   workState, workOwner, workEpoch, releasedWorkEpoch,
                   realizationReady, committedDisposition, committedOwner,
                   committedEpoch, sessionObservationApplied>>

\* ThreadCommit is the sole Run-disposition authority. Dispatch remains leased
\* so commit -> observer -> settlement crashes stay explicit.
Commit(run, disposition, candidate, epoch) ==
    /\ disposition \in {"Awaiting", "Ended"}
    /\ committedDisposition[run] = "None"
    /\ run \in executionStarted
    /\ DState(run) = "Leased"
    /\ DOwner(run) = candidate
    /\ DEpoch(run) = epoch
    /\ committedDisposition' = [committedDisposition EXCEPT ![run] = disposition]
    /\ committedOwner' = [committedOwner EXCEPT ![run] = candidate]
    /\ committedEpoch' = [committedEpoch EXCEPT ![run] = epoch]
    /\ executionStarted' = executionStarted \ {run}
    /\ UNCHANGED <<dispatchVars, activityEpoch, activeActivityEpochs,
                   nextActivityEpoch, workState, workOwner, workEpoch,
                   releasedWorkEpoch, realizationReady,
                   sessionObservationApplied>>

ApplySessionObservation(run) ==
    /\ committedDisposition[run] \in {"Awaiting", "Ended"}
    /\ run \notin sessionObservationApplied
    /\ activityEpoch[run] \in activeActivityEpochs
    /\ sessionObservationApplied' = sessionObservationApplied \cup {run}
    /\ activeActivityEpochs' = activeActivityEpochs \ {activityEpoch[run]}
    /\ UNCHANGED <<dispatchVars, activityEpoch, nextActivityEpoch,
                   workState, workOwner, workEpoch, releasedWorkEpoch,
                   realizationReady, executionStarted, committedDisposition,
                   committedOwner, committedEpoch>>

\* Only a root Session Run owns Work release; children retain the borrowed
\* fence. Quiescence preserves already-claimed child attempts during root close.
ReleaseWork(run, candidate, epoch) ==
    /\ run \in RootRuns
    /\ run \in sessionObservationApplied
    /\ executionStarted = {}
    /\ workState = "Leased"
    /\ workOwner = candidate
    /\ workEpoch = epoch
    /\ workState' = "Released"
    /\ workOwner' = NoWorker
    /\ releasedWorkEpoch' = epoch
    /\ realizationReady' = FALSE
    /\ UNCHANGED <<dispatchVars, activityEpoch, activeActivityEpochs,
                   nextActivityEpoch, workEpoch, executionStarted,
                   committedDisposition, committedOwner, committedEpoch,
                   sessionObservationApplied>>

RootSettlementReady(run) == (run \notin RootRuns) \/ workState = "Released"

SettleAwaiting(run, candidate, epoch) ==
    /\ committedDisposition[run] = "Awaiting"
    /\ run \in sessionObservationApplied
    /\ RootSettlementReady(run)
    /\ SettleAwaitingDispatch(run, candidate, epoch)
    /\ executionStarted' = executionStarted \ {run}
    /\ UNCHANGED <<activityEpoch, activeActivityEpochs, nextActivityEpoch,
                   workState, workOwner, workEpoch, releasedWorkEpoch,
                   realizationReady, committedDisposition, committedOwner,
                   committedEpoch, sessionObservationApplied>>

SettleDone(run, candidate, epoch) ==
    /\ committedDisposition[run] = "Ended"
    /\ run \in sessionObservationApplied
    /\ RootSettlementReady(run)
    /\ SettleDoneDispatch(run, candidate, epoch)
    /\ executionStarted' = executionStarted \ {run}
    /\ UNCHANGED <<activityEpoch, activeActivityEpochs, nextActivityEpoch,
                   workState, workOwner, workEpoch, releasedWorkEpoch,
                   realizationReady, committedDisposition, committedOwner,
                   committedEpoch, sessionObservationApplied>>

StaleRelease(candidate, epoch) ==
    /\ candidate \in Workers
    /\ epoch \in 0..MaxEpoch
    /\ \/ workState # "Leased"
       \/ candidate # workOwner
       \/ epoch # workEpoch
    /\ UNCHANGED vars

Next ==
    \/ \E run \in Runs: OpenActivity(run)
    \/ \E run \in Runs: ActivateReservation(run)
    \/ \E w \in Workers: ClaimWork(w)
    \/ ExpireWork
    \/ \E w \in Workers, e \in 0..MaxEpoch: Realize(w, e)
    \/ \E run \in Runs, w \in Workers: ClaimRun(run, w)
    \/ \E run \in Runs, w \in Workers: ReclaimRun(run, w)
    \/ \E run \in Runs, d \in {"Awaiting", "Ended"},
          w \in Workers, e \in 0..MaxEpoch: Commit(run, d, w, e)
    \/ \E run \in Runs: ApplySessionObservation(run)
    \/ \E run \in Runs, w \in Workers, e \in 0..MaxEpoch:
           ReleaseWork(run, w, e)
    \/ \E run \in Runs, w \in Workers, e \in 0..MaxEpoch:
           SettleAwaiting(run, w, e)
    \/ \E run \in Runs, w \in Workers, e \in 0..MaxEpoch:
           SettleDone(run, w, e)
    \/ \E w \in Workers, e \in 0..MaxEpoch: StaleRelease(w, e)

Spec == Init /\ [][Next]_vars

TypeOK ==
    /\ RootRun # ChildRunA
    /\ RootRun # ChildRunB
    /\ ChildRunA # ChildRunB
    /\ RootThread # ChildThread
    /\ activityEpoch \in [Runs -> 0..MaxEpoch]
    /\ activeActivityEpochs \subseteq 1..MaxEpoch
    /\ nextActivityEpoch \in 1..(MaxEpoch + 1)
    /\ workState \in {"Queued", "Leased", "Released"}
    /\ workOwner \in Workers \cup {NoWorker}
    /\ workEpoch \in 0..MaxEpoch
    /\ releasedWorkEpoch \in 0..MaxEpoch
    /\ realizationReady \in BOOLEAN
    /\ executionStarted \subseteq Runs
    /\ committedDisposition \in [Runs -> {"None", "Awaiting", "Ended"}]
    /\ committedOwner \in [Runs -> (Workers \cup {NoWorker})]
    /\ committedEpoch \in [Runs -> 0..MaxEpoch]
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
            run \notin executionStarted

ExecutionRequiresAllAuthorities ==
    \A run \in executionStarted:
        /\ DState(run) = "Leased"
        /\ DOwner(run) = workOwner
        /\ workState = "Leased"
        /\ realizationReady

OneExecutionAuthorityPerThread ==
    \A left, right \in executionStarted:
        RunThread(left) = RunThread(right) => left = right

CommitUsesExactClaim ==
    \A run \in Runs:
        committedDisposition[run] # "None" =>
            /\ committedEpoch[run] > 0
            /\ committedOwner[run] \in Workers

ObservationRequiresCommit ==
    \A run \in sessionObservationApplied:
        committedDisposition[run] # "None"

SettlementRequiresObservation ==
    \A run \in Runs:
        DState(run) \in {"Awaiting", "Removed"} =>
            run \in sessionObservationApplied

ReleasedWorkIsEpochFenced == workState = "Released" =>
    /\ workOwner = NoWorker
    /\ releasedWorkEpoch > 0
    /\ executionStarted = {}

Safety ==
    /\ TypeOK
    /\ IngressSafety
    /\ ActivityBeforeExecutableDispatch
    /\ ReservationCannotExecute
    /\ ExecutionRequiresAllAuthorities
    /\ OneExecutionAuthorityPerThread
    /\ CommitUsesExactClaim
    /\ ObservationRequiresCommit
    /\ SettlementRequiresObservation
    /\ ReleasedWorkIsEpochFenced

=============================================================================
