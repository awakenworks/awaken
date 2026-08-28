------------------------------ MODULE RunIngress ------------------------------
EXTENDS Naturals

\* Standalone executable specification of one Dispatch row. Every transition
\* delegates to RunIngressKernel, which is also consumed by the multi-Run
\* Session protocol. Run disposition, tickets, ThreadCommit, ToolBatch and
\* Session activity remain external authorities.
CONSTANTS Owners, NoOwner, MaxEpoch

VARIABLES dispatchState, owner, leaseEpoch, cancelRequested, pendingInput

vars == <<dispatchState, owner, leaseEpoch, cancelRequested, pendingInput>>

Kernel == INSTANCE RunIngressKernel WITH
    Owners <- Owners,
    NoOwner <- NoOwner,
    MaxEpoch <- MaxEpoch,
    kState <- dispatchState,
    kOwner <- owner,
    kLeaseEpoch <- leaseEpoch,
    kCancelRequested <- cancelRequested,
    kPendingInput <- pendingInput

Init == Kernel!Init

ClaimReservation(candidate) == Kernel!ClaimReservation(candidate)
ActivateReservation == Kernel!ActivateReservation
RejectUnclaimedReservation == Kernel!RejectReservation

ResolveReservation(candidate, epoch) ==
    Kernel!ResolveReservation(candidate, epoch, "Activate")

RetryReservation(candidate, epoch) ==
    Kernel!ResolveReservation(candidate, epoch, "Retry")

RejectReservation(candidate, epoch) ==
    Kernel!ResolveReservation(candidate, epoch, "Reject")

Claim(candidate) ==
    /\ dispatchState \in {"Pending", "Awaiting"}
    /\ Kernel!Claim(candidate)

Reclaim(candidate) ==
    /\ dispatchState = "Leased"
    /\ Kernel!Claim(candidate)

Relinquish(candidate, epoch) == Kernel!Relinquish(candidate, epoch)
DeliverInput == Kernel!DeliverInput
SettleAwaiting(candidate, epoch) == Kernel!SettleAwaiting(candidate, epoch)
SettleDone(candidate, epoch) == Kernel!SettleDone(candidate, epoch)
RequestCancel == Kernel!Cancel
ExhaustRetries(candidate, epoch) == Kernel!ExhaustRetries(candidate, epoch)
RequeueDeadLetter == Kernel!RequeueDeadLetter
Supersede == Kernel!Supersede

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
    \/ RejectUnclaimedReservation
    \/ \E candidate \in Owners, epoch \in 0..MaxEpoch:
           ResolveReservation(candidate, epoch)
    \/ \E candidate \in Owners, epoch \in 0..MaxEpoch:
           RetryReservation(candidate, epoch)
    \/ \E candidate \in Owners, epoch \in 0..MaxEpoch:
           RejectReservation(candidate, epoch)
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

TypeInvariant == Kernel!TypeOK
LeaseOwnerInvariant == Kernel!LeaseHasExactlyOneOwner
TerminalOwnerInvariant == Kernel!TerminalHasNoOwner
LeasedEpochInvariant == Kernel!LeasedEpochIsPositive
PendingInputInvariant == Kernel!PendingInputIsDurable

=============================================================================
