-------------------------- MODULE RunIngressKernel --------------------------
EXTENDS Naturals

\* Parameterized Dispatch-row transition kernel. Standalone RunIngress and
\* SessionRunProtocol instantiate these variables onto their own state so the
\* executable transition relation has one authority.
CONSTANTS Owners, NoOwner, MaxEpoch

DispatchStates == {
    "Reserved", "ReservationLeased", "Pending", "Leased", "Awaiting",
    "DeadLetter", "Removed", "Superseded"
}
LeasedStates == {"ReservationLeased", "Leased"}
TerminalDispatchStates == {"Removed", "Superseded"}

VARIABLES
    kState,
    kOwner,
    kLeaseEpoch,
    kCancelRequested,
    kPendingInput

kVars == <<kState, kOwner, kLeaseEpoch, kCancelRequested, kPendingInput>>

Init ==
    /\ kState = "Reserved"
    /\ kOwner = NoOwner
    /\ kLeaseEpoch = 0
    /\ kCancelRequested = FALSE
    /\ kPendingInput = FALSE

ClaimReservation(candidate) ==
    /\ candidate \in Owners
    /\ kState = "Reserved"
    /\ kLeaseEpoch < MaxEpoch
    /\ kState' = "ReservationLeased"
    /\ kOwner' = candidate
    /\ kLeaseEpoch' = kLeaseEpoch + 1
    /\ UNCHANGED <<kCancelRequested, kPendingInput>>

ActivateReservation ==
    /\ kState = "Reserved"
    /\ kState' = "Pending"
    /\ UNCHANGED <<kOwner, kLeaseEpoch, kCancelRequested, kPendingInput>>

RejectReservation ==
    /\ kState = "Reserved"
    /\ kState' = "Removed"
    /\ kOwner' = NoOwner
    /\ kPendingInput' = FALSE
    /\ UNCHANGED <<kLeaseEpoch, kCancelRequested>>

ResolveReservation(candidate, epoch, resolution) ==
    /\ resolution \in {"Activate", "Retry", "Reject"}
    /\ kState = "ReservationLeased"
    /\ kOwner = candidate
    /\ kLeaseEpoch = epoch
    /\ kState' = CASE resolution = "Activate" -> "Pending"
                    [] resolution = "Retry" -> "Reserved"
                    [] OTHER -> "Removed"
    /\ kOwner' = NoOwner
    /\ IF resolution = "Reject"
          THEN /\ kPendingInput' = FALSE
               /\ UNCHANGED <<kLeaseEpoch, kCancelRequested>>
          ELSE UNCHANGED <<kLeaseEpoch, kCancelRequested, kPendingInput>>

Claim(candidate) ==
    /\ candidate \in Owners
    /\ kState \in {"Pending", "Awaiting", "Leased"}
    /\ kLeaseEpoch < MaxEpoch
    /\ kState' = "Leased"
    /\ kOwner' = candidate
    /\ kLeaseEpoch' = kLeaseEpoch + 1
    /\ UNCHANGED <<kCancelRequested, kPendingInput>>

Relinquish(candidate, epoch) ==
    /\ kState = "Leased"
    /\ kOwner = candidate
    /\ kLeaseEpoch = epoch
    /\ kState' = "Pending"
    /\ kOwner' = NoOwner
    /\ UNCHANGED <<kLeaseEpoch, kCancelRequested, kPendingInput>>

DeliverInput ==
    /\ kState = "Awaiting"
    /\ ~kPendingInput
    /\ kPendingInput' = TRUE
    /\ UNCHANGED <<kState, kOwner, kLeaseEpoch, kCancelRequested>>

SettleAwaiting(candidate, epoch) ==
    /\ kState = "Leased"
    /\ kOwner = candidate
    /\ kLeaseEpoch = epoch
    /\ kState' = "Awaiting"
    /\ kOwner' = NoOwner
    /\ kPendingInput' = FALSE
    /\ UNCHANGED <<kLeaseEpoch, kCancelRequested>>

SettleDone(candidate, epoch) ==
    /\ kState = "Leased"
    /\ kOwner = candidate
    /\ kLeaseEpoch = epoch
    /\ kState' = "Removed"
    /\ kOwner' = NoOwner
    /\ kPendingInput' = FALSE
    /\ UNCHANGED <<kLeaseEpoch, kCancelRequested>>

Cancel ==
    /\ kState \notin TerminalDispatchStates
    /\ kCancelRequested' = TRUE
    /\ IF kState \in LeasedStates
          THEN /\ kLeaseEpoch < MaxEpoch
               /\ kState' = IF kState = "ReservationLeased"
                                THEN "Reserved" ELSE "Pending"
               /\ kOwner' = NoOwner
               /\ kLeaseEpoch' = kLeaseEpoch + 1
               /\ UNCHANGED kPendingInput
          ELSE UNCHANGED <<kState, kOwner, kLeaseEpoch, kPendingInput>>

ExhaustRetries(candidate, epoch) ==
    /\ kState = "Leased"
    /\ kOwner = candidate
    /\ kLeaseEpoch = epoch
    /\ kState' = "DeadLetter"
    /\ kOwner' = NoOwner
    /\ UNCHANGED <<kLeaseEpoch, kCancelRequested, kPendingInput>>

RequeueDeadLetter ==
    /\ kState = "DeadLetter"
    /\ ~kCancelRequested
    /\ kState' = "Pending"
    /\ kOwner' = NoOwner
    /\ UNCHANGED <<kLeaseEpoch, kCancelRequested, kPendingInput>>

Supersede ==
    /\ kState \in {"Pending", "Awaiting"}
    /\ ~kCancelRequested
    /\ kState' = "Superseded"
    /\ kOwner' = NoOwner
    /\ kPendingInput' = FALSE
    /\ UNCHANGED <<kLeaseEpoch, kCancelRequested>>

TypeOK ==
    /\ kState \in DispatchStates
    /\ kOwner \in Owners \cup {NoOwner}
    /\ kLeaseEpoch \in 0..MaxEpoch
    /\ kCancelRequested \in BOOLEAN
    /\ kPendingInput \in BOOLEAN

LeaseHasExactlyOneOwner ==
    (kState \in LeasedStates) \equiv (kOwner \in Owners)

TerminalHasNoOwner ==
    kState \in TerminalDispatchStates => kOwner = NoOwner

LeasedEpochIsPositive ==
    kState \in LeasedStates => kLeaseEpoch > 0

PendingInputIsDurable ==
    kPendingInput => kState \in {"Pending", "Awaiting", "Leased", "DeadLetter"}

Safety ==
    /\ TypeOK
    /\ LeaseHasExactlyOneOwner
    /\ TerminalHasNoOwner
    /\ LeasedEpochIsPositive
    /\ PendingInputIsDurable

=============================================================================
