------------------------- MODULE SessionEventKernel -------------------------
EXTENDS Naturals

\* One create-time User Event projected through the existing activity and
\* Dispatch kernels. The Event root commit precedes reservation; an outer
\* existence bit gates the preinitialized Dispatch kernel until the durable row
\* is actually inserted. ThreadCommit remains an observed external authority.
CONSTANTS EventOperation, RunOperation, Workers, NoWorker, MaxEpoch

ActivityIds == {EventOperation, RunOperation}

EventPhases == {"None", "Accepted", "Reserved", "Activated", "Anchored", "Processed", "Rejected"}
CommittedDispositions == {"None", "Awaiting", "Ended"}

VARIABLES
    kDispatchExists,
    kDispatchState,
    kDispatchOwner,
    kDispatchEpoch,
    kDispatchCancel,
    kDispatchInput,
    kActivityEpoch,
    kActiveActivityEpochs,
    kSettledActivities,
    kNextActivityEpoch,
    kEventPhase,
    kCommittedDisposition,
    kProjectionAnchor

dispatchVars == <<kDispatchState, kDispatchOwner, kDispatchEpoch,
                  kDispatchCancel, kDispatchInput>>
activityVars == <<kActivityEpoch, kActiveActivityEpochs,
                  kSettledActivities, kNextActivityEpoch>>
kVars == <<kDispatchExists, dispatchVars, activityVars, kEventPhase,
           kCommittedDisposition, kProjectionAnchor>>

Ingress == INSTANCE RunIngressKernel WITH
    Owners <- Workers,
    NoOwner <- NoWorker,
    MaxEpoch <- MaxEpoch,
    kState <- kDispatchState,
    kOwner <- kDispatchOwner,
    kLeaseEpoch <- kDispatchEpoch,
    kCancelRequested <- kDispatchCancel,
    kPendingInput <- kDispatchInput

Activity == INSTANCE SessionActivityKernel WITH
    ActivityIds <- ActivityIds,
    MaxActivityEpoch <- MaxEpoch,
    kActivityEpoch <- kActivityEpoch,
    kActiveActivityEpochs <- kActiveActivityEpochs,
    kSettledActivities <- kSettledActivities,
    kNextActivityEpoch <- kNextActivityEpoch

Init ==
    /\ Ingress!Init
    /\ Activity!Init
    /\ kDispatchExists = FALSE
    /\ kEventPhase = "None"
    /\ kCommittedDisposition = "None"
    /\ kProjectionAnchor = FALSE

AcceptInitialEvent ==
    /\ kEventPhase = "None"
    /\ ~kDispatchExists
    /\ Activity!Open(EventOperation)
    /\ kEventPhase' = "Accepted"
    /\ UNCHANGED <<kDispatchExists, dispatchVars,
                   kCommittedDisposition, kProjectionAnchor>>

ReserveRun ==
    /\ kEventPhase = "Accepted"
    /\ ~kDispatchExists
    /\ kDispatchExists' = TRUE
    /\ kEventPhase' = "Reserved"
    /\ UNCHANGED <<dispatchVars, activityVars,
                   kCommittedDisposition, kProjectionAnchor>>

ReplayReserve ==
    /\ kDispatchExists
    /\ kEventPhase \in {"Reserved", "Activated", "Anchored", "Processed"}
    /\ UNCHANGED kVars

\* Reservation is deliberately durable before the Session root records the
\* Run-specific activity receipt. A crash in this window is repaired from the
\* non-executable reservation; it must never be collapsed into one action.
RegisterRunActivity ==
    /\ kDispatchExists
    /\ kEventPhase = "Reserved"
    /\ Activity!Open(RunOperation)
    /\ UNCHANGED <<kDispatchExists, dispatchVars, kEventPhase,
                   kCommittedDisposition, kProjectionAnchor>>

ActivateRun ==
    /\ kDispatchExists
    /\ kEventPhase = "Reserved"
    /\ kActivityEpoch[RunOperation] > 0
    /\ Ingress!ActivateReservation
    /\ kEventPhase' = "Activated"
    /\ UNCHANGED <<kDispatchExists, activityVars,
                   kCommittedDisposition, kProjectionAnchor>>

ClaimReservationRepair(worker) ==
    /\ kDispatchExists
    /\ kEventPhase = "Reserved"
    /\ Ingress!ClaimReservation(worker)
    /\ UNCHANGED <<kDispatchExists, activityVars, kEventPhase,
                   kCommittedDisposition, kProjectionAnchor>>

ResolveReservationRepair(worker, epoch, resolution) ==
    /\ resolution \in {"Activate", "Retry", "Reject"}
    /\ kDispatchExists
    /\ kEventPhase = "Reserved"
    /\ (resolution = "Activate" => kActivityEpoch[RunOperation] > 0)
    /\ Ingress!ResolveReservation(worker, epoch, resolution)
    /\ kEventPhase' = CASE resolution = "Activate" -> "Activated"
                         [] resolution = "Reject" -> "Rejected"
                         [] OTHER -> "Reserved"
    /\ UNCHANGED <<kDispatchExists, activityVars,
                   kCommittedDisposition, kProjectionAnchor>>

RejectUnclaimedReservation ==
    /\ kDispatchExists
    /\ kEventPhase = "Reserved"
    /\ Ingress!RejectReservation
    /\ kEventPhase' = "Rejected"
    /\ UNCHANGED <<kDispatchExists, activityVars,
                   kCommittedDisposition, kProjectionAnchor>>

SettleRejectedRunActivity ==
    /\ kEventPhase = "Rejected"
    /\ Activity!Settle(RunOperation)
    /\ UNCHANGED <<kDispatchExists, dispatchVars, kEventPhase,
                   kCommittedDisposition, kProjectionAnchor>>

ClaimRun(worker) ==
    /\ kDispatchExists
    /\ kEventPhase = "Activated"
    /\ Ingress!Claim(worker)
    /\ UNCHANGED <<kDispatchExists, activityVars, kEventPhase,
                   kCommittedDisposition, kProjectionAnchor>>

\* The production ThreadCommit is the only writer of disposition. This action
\* records the exact committed fact observed under the current Dispatch claim;
\* it intentionally does not mutate Dispatch.
ObserveCommittedRun(worker, epoch, disposition) ==
    /\ disposition \in {"Awaiting", "Ended"}
    /\ kEventPhase = "Activated"
    /\ kCommittedDisposition = "None"
    /\ kDispatchState = "Leased"
    /\ kDispatchOwner = worker
    /\ kDispatchEpoch = epoch
    /\ kCommittedDisposition' = disposition
    /\ kProjectionAnchor' = TRUE
    /\ kEventPhase' = "Anchored"
    /\ UNCHANGED <<kDispatchExists, dispatchVars, activityVars>>

\* The committed Run observer applies the idempotent Session activity
\* settlement before destructively settling Dispatch. This preserves the row
\* as the recovery correlation until the Session projection is durable.
SettleRunActivity ==
    /\ kEventPhase = "Anchored"
    /\ kProjectionAnchor
    /\ Activity!Settle(RunOperation)
    /\ UNCHANGED <<kDispatchExists, dispatchVars, kEventPhase,
                   kCommittedDisposition, kProjectionAnchor>>

SettleCommittedRun(worker, epoch) ==
    /\ kEventPhase = "Anchored"
    /\ kProjectionAnchor
    /\ IF kCommittedDisposition = "Awaiting"
          THEN Ingress!SettleAwaiting(worker, epoch)
          ELSE /\ kCommittedDisposition = "Ended"
               /\ Ingress!SettleDone(worker, epoch)
    /\ UNCHANGED <<kDispatchExists, activityVars, kEventPhase,
                   kCommittedDisposition, kProjectionAnchor>>

MarkProcessed ==
    /\ kEventPhase = "Anchored"
    /\ kProjectionAnchor
    /\ kCommittedDisposition \in {"Awaiting", "Ended"}
    /\ kDispatchState \in {"Awaiting", "Removed"}
    /\ RunOperation \in kSettledActivities
    /\ kEventPhase' = "Processed"
    /\ UNCHANGED <<kDispatchExists, dispatchVars, activityVars,
                   kCommittedDisposition, kProjectionAnchor>>

ReplayProcessed ==
    /\ kEventPhase = "Processed"
    /\ UNCHANGED kVars

SettleInitialEventWake ==
    /\ kEventPhase \in {"Processed", "Rejected"}
    /\ Activity!Settle(EventOperation)
    /\ UNCHANGED <<kDispatchExists, dispatchVars, kEventPhase,
                   kCommittedDisposition, kProjectionAnchor>>

Next ==
    \/ AcceptInitialEvent
    \/ ReserveRun
    \/ ReplayReserve
    \/ RegisterRunActivity
    \/ ActivateRun
    \/ \E worker \in Workers: ClaimReservationRepair(worker)
    \/ \E worker \in Workers, epoch \in 0..MaxEpoch,
          resolution \in {"Activate", "Retry", "Reject"}:
           ResolveReservationRepair(worker, epoch, resolution)
    \/ RejectUnclaimedReservation
    \/ SettleRejectedRunActivity
    \/ \E worker \in Workers: ClaimRun(worker)
    \/ \E worker \in Workers, epoch \in 0..MaxEpoch,
          disposition \in {"Awaiting", "Ended"}:
           ObserveCommittedRun(worker, epoch, disposition)
    \/ SettleRunActivity
    \/ \E worker \in Workers, epoch \in 0..MaxEpoch:
           SettleCommittedRun(worker, epoch)
    \/ MarkProcessed
    \/ ReplayProcessed
    \/ SettleInitialEventWake

Spec == Init /\ [][Next]_kVars

TypeOK ==
    /\ kDispatchExists \in BOOLEAN
    /\ kEventPhase \in EventPhases
    /\ kCommittedDisposition \in CommittedDispositions
    /\ kProjectionAnchor \in BOOLEAN
    /\ Ingress!TypeOK
    /\ Activity!TypeOK

DispatchRequiresAcceptedEvent ==
    kDispatchExists => kEventPhase # "None"

ExecutableDispatchRequiresActivityReceipt ==
    kDispatchExists /\ kDispatchState \in {"Pending", "Leased", "Awaiting"} =>
        kActivityEpoch[RunOperation] > 0

ReservationCannotExecute ==
    kDispatchState \in {"Reserved", "ReservationLeased"} =>
        kCommittedDisposition = "None"

AnchorRequiresExactCommit ==
    kProjectionAnchor <=> kCommittedDisposition \in {"Awaiting", "Ended"}

ProcessedRequiresDurableCausalChain ==
    kEventPhase = "Processed" =>
        /\ kDispatchExists
        /\ kProjectionAnchor
        /\ RunOperation \in kSettledActivities
        /\ kDispatchState \in {"Awaiting", "Removed"}

RejectedNeverBecomesExecutable ==
    kEventPhase = "Rejected" =>
        kDispatchState = "Removed"

Safety ==
    /\ TypeOK
    /\ Ingress!Safety
    /\ Activity!Safety
    /\ DispatchRequiresAcceptedEvent
    /\ ExecutableDispatchRequiresActivityReceipt
    /\ ReservationCannotExecute
    /\ AnchorRequiresExactCommit
    /\ ProcessedRequiresDurableCausalChain
    /\ RejectedNeverBecomesExecutable

=============================================================================
