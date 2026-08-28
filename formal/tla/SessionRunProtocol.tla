------------------------- MODULE SessionRunProtocol -------------------------
EXTENDS Naturals

\* Composition of existing authority boundaries. Dispatch transitions are the
\* RunIngress instance below; this protocol adds only Session activity, Work
\* lease, realization, committed-Thread observation and their ordering. It does
\* not define another Run, Thread, ticket, or Tool state machine.
CONSTANTS Workers, NoWorker, MaxEpoch

VARIABLES
    dispatchState, owner, leaseEpoch, cancelRequested, pendingInput,
    activityEpochs, nextActivityEpoch,
    workState, workOwner, workEpoch, releasedWorkEpoch,
    realizationReady,
    executionStarted,
    committedDisposition, committedOwner, committedEpoch,
    sessionObservationApplied

ingressVars == <<dispatchState, owner, leaseEpoch, cancelRequested, pendingInput>>
protocolVars == <<activityEpochs, nextActivityEpoch,
                  workState, workOwner, workEpoch, releasedWorkEpoch,
                  realizationReady, executionStarted,
                  committedDisposition, committedOwner, committedEpoch,
                  sessionObservationApplied>>
vars == <<ingressVars, protocolVars>>

Ingress == INSTANCE RunIngress
    WITH Owners <- Workers,
         NoOwner <- NoWorker

Init ==
    /\ Ingress!Init
    /\ activityEpochs = {}
    /\ nextActivityEpoch = 1
    /\ workState = "Queued"
    /\ workOwner = NoWorker
    /\ workEpoch = 0
    /\ releasedWorkEpoch = 0
    /\ realizationReady = FALSE
    /\ executionStarted = FALSE
    /\ committedDisposition = "None"
    /\ committedOwner = NoWorker
    /\ committedEpoch = 0
    /\ sessionObservationApplied = FALSE

OpenActivity ==
    /\ dispatchState = "Reserved"
    /\ activityEpochs = {}
    /\ nextActivityEpoch <= MaxEpoch
    /\ activityEpochs' = {nextActivityEpoch}
    /\ nextActivityEpoch' = nextActivityEpoch + 1
    /\ UNCHANGED <<ingressVars, workState, workOwner, workEpoch,
                   releasedWorkEpoch, realizationReady, executionStarted,
                   committedDisposition, committedOwner, committedEpoch,
                   sessionObservationApplied>>

ActivateReservation ==
    /\ activityEpochs # {}
    /\ Ingress!ActivateReservation
    /\ UNCHANGED protocolVars

ClaimWork(candidate) ==
    /\ candidate \in Workers
    /\ workState = "Queued"
    /\ workEpoch < MaxEpoch
    /\ workState' = "Leased"
    /\ workOwner' = candidate
    /\ workEpoch' = workEpoch + 1
    /\ UNCHANGED <<ingressVars, activityEpochs, nextActivityEpoch,
                   releasedWorkEpoch, realizationReady, executionStarted,
                   committedDisposition, committedOwner, committedEpoch,
                   sessionObservationApplied>>

ExpireWork ==
    /\ workState = "Leased"
    /\ workState' = "Queued"
    /\ workOwner' = NoWorker
    /\ realizationReady' = FALSE
    /\ executionStarted' = FALSE
    /\ UNCHANGED <<ingressVars, activityEpochs, nextActivityEpoch,
                   workEpoch, releasedWorkEpoch,
                   committedDisposition, committedOwner, committedEpoch,
                   sessionObservationApplied>>

Realize(candidate, epoch) ==
    /\ workState = "Leased"
    /\ workOwner = candidate
    /\ workEpoch = epoch
    /\ realizationReady' = TRUE
    /\ UNCHANGED <<ingressVars, activityEpochs, nextActivityEpoch,
                   workState, workOwner, workEpoch, releasedWorkEpoch,
                   executionStarted, committedDisposition, committedOwner,
                   committedEpoch, sessionObservationApplied>>

ClaimRun(candidate) ==
    /\ committedDisposition = "None"
    /\ workState = "Leased"
    /\ workOwner = candidate
    /\ realizationReady
    /\ Ingress!Claim(candidate)
    /\ executionStarted' = TRUE
    /\ UNCHANGED <<activityEpochs, nextActivityEpoch,
                   workState, workOwner, workEpoch, releasedWorkEpoch,
                   realizationReady, committedDisposition, committedOwner,
                   committedEpoch, sessionObservationApplied>>

ReclaimRun(candidate) ==
    /\ workState = "Leased"
    /\ workOwner = candidate
    /\ realizationReady
    /\ Ingress!Reclaim(candidate)
    /\ executionStarted' = (committedDisposition = "None")
    /\ UNCHANGED <<activityEpochs, nextActivityEpoch,
                   workState, workOwner, workEpoch, releasedWorkEpoch,
                   realizationReady, committedDisposition, committedOwner,
                   committedEpoch, sessionObservationApplied>>

\* ThreadCommit is the sole Run-disposition authority. This step deliberately
\* leaves Dispatch leased so TLC explores commit -> observer/settle crashes.
Commit(disposition, candidate, epoch) ==
    /\ disposition \in {"Awaiting", "Ended"}
    /\ committedDisposition = "None"
    /\ executionStarted
    /\ dispatchState = "Leased"
    /\ owner = candidate
    /\ leaseEpoch = epoch
    /\ committedDisposition' = disposition
    /\ committedOwner' = candidate
    /\ committedEpoch' = epoch
    /\ executionStarted' = FALSE
    /\ UNCHANGED <<ingressVars, activityEpochs, nextActivityEpoch,
                   workState, workOwner, workEpoch, releasedWorkEpoch,
                   realizationReady, sessionObservationApplied>>

ApplySessionObservation ==
    /\ committedDisposition \in {"Awaiting", "Ended"}
    /\ ~sessionObservationApplied
    /\ activityEpochs # {}
    /\ sessionObservationApplied' = TRUE
    /\ activityEpochs' = {}
    /\ UNCHANGED <<ingressVars, nextActivityEpoch,
                   workState, workOwner, workEpoch, releasedWorkEpoch,
                   realizationReady, executionStarted, committedDisposition,
                   committedOwner, committedEpoch>>

ReleaseWork(candidate, epoch) ==
    /\ sessionObservationApplied
    /\ workState = "Leased"
    /\ workOwner = candidate
    /\ workEpoch = epoch
    /\ workState' = "Released"
    /\ workOwner' = NoWorker
    /\ releasedWorkEpoch' = epoch
    /\ realizationReady' = FALSE
    /\ UNCHANGED <<ingressVars, activityEpochs, nextActivityEpoch,
                   workEpoch, executionStarted, committedDisposition,
                   committedOwner, committedEpoch, sessionObservationApplied>>

SettleAwaiting(candidate, epoch) ==
    /\ committedDisposition = "Awaiting"
    /\ sessionObservationApplied
    /\ workState = "Released"
    /\ Ingress!SettleAwaiting(candidate, epoch)
    /\ executionStarted' = FALSE
    /\ UNCHANGED <<activityEpochs, nextActivityEpoch,
                   workState, workOwner, workEpoch, releasedWorkEpoch,
                   realizationReady, committedDisposition, committedOwner,
                   committedEpoch, sessionObservationApplied>>

SettleDone(candidate, epoch) ==
    /\ committedDisposition = "Ended"
    /\ sessionObservationApplied
    /\ workState = "Released"
    /\ Ingress!SettleDone(candidate, epoch)
    /\ executionStarted' = FALSE
    /\ UNCHANGED <<activityEpochs, nextActivityEpoch,
                   workState, workOwner, workEpoch, releasedWorkEpoch,
                   realizationReady, committedDisposition, committedOwner,
                   committedEpoch, sessionObservationApplied>>

\* A stale Work release is an explicit no-op. Same-owner higher-epoch takeover
\* is therefore covered, not only takeover by a differently named Worker.
StaleRelease(candidate, epoch) ==
    /\ candidate \in Workers
    /\ epoch \in 0..MaxEpoch
    /\ \/ workState # "Leased"
       \/ candidate # workOwner
       \/ epoch # workEpoch
    /\ UNCHANGED vars

Next ==
    \/ OpenActivity
    \/ ActivateReservation
    \/ \E w \in Workers: ClaimWork(w)
    \/ ExpireWork
    \/ \E w \in Workers, e \in 0..MaxEpoch: Realize(w, e)
    \/ \E w \in Workers: ClaimRun(w)
    \/ \E w \in Workers: ReclaimRun(w)
    \/ \E d \in {"Awaiting", "Ended"}, w \in Workers, e \in 0..MaxEpoch:
           Commit(d, w, e)
    \/ ApplySessionObservation
    \/ \E w \in Workers, e \in 0..MaxEpoch: ReleaseWork(w, e)
    \/ \E w \in Workers, e \in 0..MaxEpoch: SettleAwaiting(w, e)
    \/ \E w \in Workers, e \in 0..MaxEpoch: SettleDone(w, e)
    \/ \E w \in Workers, e \in 0..MaxEpoch: StaleRelease(w, e)

Spec == Init /\ [][Next]_vars

TypeOK ==
    /\ Ingress!TypeOK
    /\ activityEpochs \subseteq 1..MaxEpoch
    /\ nextActivityEpoch \in 1..(MaxEpoch + 1)
    /\ workState \in {"Queued", "Leased", "Released"}
    /\ workOwner \in Workers \cup {NoWorker}
    /\ workEpoch \in 0..MaxEpoch
    /\ releasedWorkEpoch \in 0..MaxEpoch
    /\ realizationReady \in BOOLEAN
    /\ executionStarted \in BOOLEAN
    /\ committedDisposition \in {"None", "Awaiting", "Ended"}
    /\ committedOwner \in Workers \cup {NoWorker}
    /\ committedEpoch \in 0..MaxEpoch
    /\ sessionObservationApplied \in BOOLEAN

ActivityBeforeExecutableDispatch ==
    dispatchState \in {"Pending", "Leased", "Awaiting", "Removed"} =>
        nextActivityEpoch > 1

ReservationCannotExecute ==
    dispatchState \in {"Reserved", "ReservationLeased"} => ~executionStarted

ExecutionRequiresAllAuthorities == executionStarted =>
    /\ dispatchState = "Leased"
    /\ owner = workOwner
    /\ workState = "Leased"
    /\ realizationReady

CommitUsesExactClaim == committedDisposition # "None" =>
    /\ committedEpoch > 0
    /\ committedOwner \in Workers

ObservationRequiresCommit == sessionObservationApplied => committedDisposition # "None"

SettlementRequiresObservation == dispatchState \in {"Awaiting", "Removed"} =>
    /\ sessionObservationApplied
    /\ activityEpochs = {}
    /\ workState = "Released"

ReleasedWorkIsEpochFenced == workState = "Released" =>
    /\ workOwner = NoWorker
    /\ releasedWorkEpoch > 0

Safety ==
    /\ TypeOK
    /\ Ingress!Safety
    /\ ActivityBeforeExecutableDispatch
    /\ ReservationCannotExecute
    /\ ExecutionRequiresAllAuthorities
    /\ CommitUsesExactClaim
    /\ ObservationRequiresCommit
    /\ SettlementRequiresObservation
    /\ ReleasedWorkIsEpochFenced

=============================================================================
