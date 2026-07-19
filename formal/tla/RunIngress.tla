------------------------------ MODULE RunIngress ------------------------------
EXTENDS Naturals

\* Runtime/run-ingress lifecycle model. WorkQueue is deliberately out of scope.
CONSTANTS Owners, NoOwner, MaxEpoch

RunStates == {"Running", "Awaiting", "Ended"}
DispatchStates == {
    "Pending",
    "Leased",
    "Awaiting",
    "Removed",
    "DeadLetter",
    "Superseded"
}
TerminalDispatchStates == {"Removed", "Superseded"}

VARIABLES
    runState,
    hasTicket,
    dispatchState,
    owner,
    leaseEpoch,
    endedOnce

vars == <<runState, hasTicket, dispatchState, owner, leaseEpoch, endedOnce>>

Init ==
    /\ runState = "Running"
    /\ hasTicket = FALSE
    /\ dispatchState = "Pending"
    /\ owner = NoOwner
    /\ leaseEpoch = 0
    /\ endedOnce = FALSE

Claim(candidate) ==
    /\ candidate \in Owners
    /\ leaseEpoch < MaxEpoch
    /\ dispatchState \in {"Pending", "Awaiting"}
    /\ IF dispatchState = "Awaiting"
          THEN /\ runState = "Awaiting" /\ hasTicket
          ELSE /\ runState = "Running" /\ ~hasTicket
    /\ runState' = "Running"
    /\ hasTicket' = FALSE
    /\ dispatchState' = "Leased"
    /\ owner' = candidate
    /\ leaseEpoch' = leaseEpoch + 1
    /\ UNCHANGED endedOnce

Reclaim(candidate) ==
    /\ candidate \in Owners
    /\ dispatchState = "Leased"
    /\ leaseEpoch < MaxEpoch
    /\ candidate # owner
    /\ owner' = candidate
    /\ leaseEpoch' = leaseEpoch + 1
    /\ UNCHANGED <<runState, hasTicket, dispatchState, endedOnce>>

SettleAwaiting(candidate, epoch) ==
    /\ dispatchState = "Leased"
    /\ owner = candidate
    /\ epoch = leaseEpoch
    /\ runState = "Running"
    /\ runState' = "Awaiting"
    /\ hasTicket' = TRUE
    /\ dispatchState' = "Awaiting"
    /\ owner' = NoOwner
    /\ UNCHANGED <<leaseEpoch, endedOnce>>

Finish(candidate, epoch) ==
    /\ dispatchState = "Leased"
    /\ owner = candidate
    /\ epoch = leaseEpoch
    /\ runState = "Running"
    /\ runState' = "Ended"
    /\ hasTicket' = FALSE
    /\ dispatchState' = "Removed"
    /\ owner' = NoOwner
    /\ endedOnce' = TRUE
    /\ UNCHANGED leaseEpoch

Cancel ==
    /\ runState # "Ended"
    /\ dispatchState \notin TerminalDispatchStates
    /\ runState' = "Ended"
    /\ hasTicket' = FALSE
    /\ dispatchState' = "Removed"
    /\ owner' = NoOwner
    /\ endedOnce' = TRUE
    /\ UNCHANGED leaseEpoch

ExhaustRetries(candidate, epoch) ==
    /\ dispatchState = "Leased"
    /\ owner = candidate
    /\ epoch = leaseEpoch
    /\ dispatchState' = "DeadLetter"
    /\ owner' = NoOwner
    /\ UNCHANGED <<runState, hasTicket, leaseEpoch, endedOnce>>

\* Dead-lettering is an operational dispatch condition, not an Agent outcome.
\* An operator may requeue it without reopening an Ended Run.
RequeueDeadLetter ==
    /\ dispatchState = "DeadLetter"
    /\ runState = "Running"
    /\ ~hasTicket
    /\ dispatchState' = "Pending"
    /\ owner' = NoOwner
    /\ UNCHANGED <<runState, hasTicket, leaseEpoch, endedOnce>>

Supersede ==
    /\ runState # "Ended"
    /\ dispatchState \notin TerminalDispatchStates
    /\ runState' = "Ended"
    /\ hasTicket' = FALSE
    /\ dispatchState' = "Superseded"
    /\ owner' = NoOwner
    /\ endedOnce' = TRUE
    /\ UNCHANGED leaseEpoch

\* A settle carrying a stale epoch is an explicit no-op. Including it in Next
\* checks the API fence without granting stale owners a state-changing action.
StaleSettle(candidate, epoch) ==
    /\ candidate \in Owners
    /\ epoch \in 0..MaxEpoch
    /\ epoch # leaseEpoch
    /\ UNCHANGED vars

Next ==
    \/ \E candidate \in Owners: Claim(candidate)
    \/ \E candidate \in Owners: Reclaim(candidate)
    \/ \E candidate \in Owners, epoch \in 0..MaxEpoch:
           SettleAwaiting(candidate, epoch)
    \/ \E candidate \in Owners, epoch \in 0..MaxEpoch:
           Finish(candidate, epoch)
    \/ \E candidate \in Owners, epoch \in 0..MaxEpoch:
           ExhaustRetries(candidate, epoch)
    \/ \E candidate \in Owners, epoch \in 0..MaxEpoch:
           StaleSettle(candidate, epoch)
    \/ Cancel
    \/ Supersede
    \/ RequeueDeadLetter

Spec == Init /\ [][Next]_vars

TypeOK ==
    /\ runState \in RunStates
    /\ hasTicket \in BOOLEAN
    /\ dispatchState \in DispatchStates
    /\ owner \in Owners \cup {NoOwner}
    /\ leaseEpoch \in 0..MaxEpoch
    /\ endedOnce \in BOOLEAN

TicketIffAwaiting == hasTicket \equiv (runState = "Awaiting")

LeaseHasExactlyOneOwner ==
    (dispatchState = "Leased") \equiv (owner \in Owners)

RunDispatchCoherence ==
    /\ (dispatchState = "Awaiting") \equiv (runState = "Awaiting")
    /\ (dispatchState \in {"Pending", "Leased"}) => (runState = "Running")
    /\ (dispatchState \in TerminalDispatchStates) => (runState = "Ended")
    /\ (dispatchState = "DeadLetter") => (runState = "Running")

EndedIsAbsorbing == endedOnce => (runState = "Ended")

LeasedEpochIsPositive == (dispatchState = "Leased") => (leaseEpoch > 0)

Safety ==
    /\ TypeOK
    /\ TicketIffAwaiting
    /\ LeaseHasExactlyOneOwner
    /\ RunDispatchCoherence
    /\ EndedIsAbsorbing
    /\ LeasedEpochIsPositive

=============================================================================
