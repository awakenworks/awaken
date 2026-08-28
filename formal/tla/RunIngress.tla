------------------------------ MODULE RunIngress ------------------------------
EXTENDS Naturals

\* Dispatch-only executable specification. Run disposition, resume tickets,
\* ThreadCommit, ToolBatch and Session activity are external authorities. This
\* module owns only queue state, pending delivery, cancellation, lease owner and
\* fencing epoch; composed protocols INSTANCE these operators instead of copying
\* their transitions.
CONSTANTS Owners, NoOwner, MaxEpoch

DispatchStates == {
    "Reserved",
    "ReservationLeased",
    "Pending",
    "Leased",
    "Awaiting",
    "DeadLetter",
    "Removed",
    "Superseded"
}
LeasedStates == {"ReservationLeased", "Leased"}
TerminalDispatchStates == {"Removed", "Superseded"}

VARIABLES dispatchState, owner, leaseEpoch, cancelRequested, pendingInput

vars == <<dispatchState, owner, leaseEpoch, cancelRequested, pendingInput>>

Init ==
    /\ dispatchState = "Reserved"
    /\ owner = NoOwner
    /\ leaseEpoch = 0
    /\ cancelRequested = FALSE
    /\ pendingInput = FALSE

ClaimReservation(candidate) ==
    /\ candidate \in Owners
    /\ dispatchState = "Reserved"
    /\ leaseEpoch < MaxEpoch
    /\ dispatchState' = "ReservationLeased"
    /\ owner' = candidate
    /\ leaseEpoch' = leaseEpoch + 1
    /\ UNCHANGED <<cancelRequested, pendingInput>>

ActivateReservation ==
    /\ dispatchState = "Reserved"
    /\ dispatchState' = "Pending"
    /\ UNCHANGED <<owner, leaseEpoch, cancelRequested, pendingInput>>

ResolveReservation(candidate, epoch) ==
    /\ dispatchState = "ReservationLeased"
    /\ owner = candidate
    /\ leaseEpoch = epoch
    /\ dispatchState' = "Pending"
    /\ owner' = NoOwner
    /\ UNCHANGED <<leaseEpoch, cancelRequested, pendingInput>>

RetryReservation(candidate, epoch) ==
    /\ dispatchState = "ReservationLeased"
    /\ owner = candidate
    /\ leaseEpoch = epoch
    /\ dispatchState' = "Reserved"
    /\ owner' = NoOwner
    /\ UNCHANGED <<leaseEpoch, cancelRequested, pendingInput>>

Claim(candidate) ==
    /\ candidate \in Owners
    /\ leaseEpoch < MaxEpoch
    /\ \/ dispatchState = "Pending"
       \/ /\ dispatchState = "Awaiting"
          /\ \/ pendingInput
             \/ cancelRequested
    /\ dispatchState' = "Leased"
    /\ owner' = candidate
    /\ leaseEpoch' = leaseEpoch + 1
    /\ UNCHANGED <<cancelRequested, pendingInput>>

Reclaim(candidate) ==
    /\ candidate \in Owners
    /\ dispatchState = "Leased"
    /\ leaseEpoch < MaxEpoch
    /\ candidate # owner
    /\ owner' = candidate
    /\ leaseEpoch' = leaseEpoch + 1
    /\ UNCHANGED <<dispatchState, cancelRequested, pendingInput>>

Relinquish(candidate, epoch) ==
    /\ dispatchState = "Leased"
    /\ owner = candidate
    /\ epoch = leaseEpoch
    /\ dispatchState' = "Pending"
    /\ owner' = NoOwner
    /\ UNCHANGED <<leaseEpoch, cancelRequested, pendingInput>>

DeliverInput ==
    /\ dispatchState = "Awaiting"
    /\ ~pendingInput
    /\ pendingInput' = TRUE
    /\ UNCHANGED <<dispatchState, owner, leaseEpoch, cancelRequested>>

\* ThreadCommit has already committed Awaiting before this queue settlement.
\* The queue consumes its pending receipt but does not create or mutate a ticket.
SettleAwaiting(candidate, epoch) ==
    /\ dispatchState = "Leased"
    /\ owner = candidate
    /\ epoch = leaseEpoch
    /\ dispatchState' = "Awaiting"
    /\ owner' = NoOwner
    /\ pendingInput' = FALSE
    /\ UNCHANGED <<leaseEpoch, cancelRequested>>

\* ThreadCommit has already committed a terminal Run disposition. Removal is a
\* later idempotent queue effect and therefore cannot erase the crash window.
SettleDone(candidate, epoch) ==
    /\ dispatchState = "Leased"
    /\ owner = candidate
    /\ epoch = leaseEpoch
    /\ dispatchState' = "Removed"
    /\ owner' = NoOwner
    /\ pendingInput' = FALSE
    /\ UNCHANGED <<leaseEpoch, cancelRequested>>

RequestCancel ==
    /\ dispatchState \notin TerminalDispatchStates
    /\ ~cancelRequested
    /\ cancelRequested' = TRUE
    /\ IF dispatchState = "Leased"
          THEN /\ dispatchState' = "Pending" /\ owner' = NoOwner
          ELSE UNCHANGED <<dispatchState, owner>>
    /\ UNCHANGED <<leaseEpoch, pendingInput>>

ExhaustRetries(candidate, epoch) ==
    /\ dispatchState = "Leased"
    /\ owner = candidate
    /\ epoch = leaseEpoch
    /\ dispatchState' = "DeadLetter"
    /\ owner' = NoOwner
    /\ UNCHANGED <<leaseEpoch, cancelRequested, pendingInput>>

RequeueDeadLetter ==
    /\ dispatchState = "DeadLetter"
    /\ dispatchState' = "Pending"
    /\ owner' = NoOwner
    /\ UNCHANGED <<leaseEpoch, cancelRequested, pendingInput>>

Supersede ==
    /\ dispatchState \notin TerminalDispatchStates
    /\ dispatchState' = "Superseded"
    /\ owner' = NoOwner
    /\ pendingInput' = FALSE
    /\ UNCHANGED <<leaseEpoch, cancelRequested>>

StaleSettle(candidate, epoch) ==
    /\ candidate \in Owners
    /\ epoch \in 0..MaxEpoch
    /\ \/ dispatchState # "Leased"
       \/ candidate # owner
       \/ epoch # leaseEpoch
    /\ UNCHANGED vars

Next ==
    \/ \E candidate \in Owners: ClaimReservation(candidate)
    \/ ActivateReservation
    \/ \E candidate \in Owners, epoch \in 0..MaxEpoch:
           ResolveReservation(candidate, epoch)
    \/ \E candidate \in Owners, epoch \in 0..MaxEpoch:
           RetryReservation(candidate, epoch)
    \/ \E candidate \in Owners: Claim(candidate)
    \/ \E candidate \in Owners: Reclaim(candidate)
    \/ \E candidate \in Owners, epoch \in 0..MaxEpoch:
           Relinquish(candidate, epoch)
    \/ DeliverInput
    \/ \E candidate \in Owners, epoch \in 0..MaxEpoch:
           SettleAwaiting(candidate, epoch)
    \/ \E candidate \in Owners, epoch \in 0..MaxEpoch:
           SettleDone(candidate, epoch)
    \/ RequestCancel
    \/ \E candidate \in Owners, epoch \in 0..MaxEpoch:
           ExhaustRetries(candidate, epoch)
    \/ RequeueDeadLetter
    \/ Supersede
    \/ \E candidate \in Owners, epoch \in 0..MaxEpoch:
           StaleSettle(candidate, epoch)

Spec == Init /\ [][Next]_vars

TypeOK ==
    /\ dispatchState \in DispatchStates
    /\ owner \in Owners \cup {NoOwner}
    /\ leaseEpoch \in 0..MaxEpoch
    /\ cancelRequested \in BOOLEAN
    /\ pendingInput \in BOOLEAN

LeaseHasExactlyOneOwner ==
    (dispatchState \in LeasedStates) \equiv (owner \in Owners)

TerminalHasNoOwner ==
    (dispatchState \in TerminalDispatchStates) => owner = NoOwner

LeasedEpochIsPositive ==
    (dispatchState \in LeasedStates) => leaseEpoch > 0

PendingInputIsDurable ==
    pendingInput => dispatchState \in {"Pending", "Awaiting", "Leased", "DeadLetter"}

Safety ==
    /\ TypeOK
    /\ LeaseHasExactlyOneOwner
    /\ TerminalHasNoOwner
    /\ LeasedEpochIsPositive
    /\ PendingInputIsDurable

=============================================================================
