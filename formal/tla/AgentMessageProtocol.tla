------------------------- MODULE AgentMessageProtocol ----------------------
EXTENDS Naturals

\* Internal Agent messaging composes the canonical Session-activity and
\* Run-ingress kernels. This module owns only cross-domain ordering: committed
\* source request, admission certainty, source receipt, target Thread commit,
\* and target-boundary Session settlement. The source receipt and target commit
\* are independent after durable admission: either may win the crash/recovery
\* race, while only the target boundary may settle the Session activity.
CONSTANTS Owners, NoOwner, MessageActivity, MaxEpoch

Activities == {MessageActivity}

VARIABLES
    dispatchState, dispatchOwner, dispatchEpoch, dispatchCancelled, dispatchInput,
    activityEpoch, activeActivityEpochs, settledActivities, nextActivityEpoch,
    requestDurable, admissionOutcome, targetMessageCommitted,
    targetBoundaryCommitted, sourceReceiptCommitted, receiptTiming, lastOutcome

dispatchVars == <<dispatchState, dispatchOwner, dispatchEpoch,
                  dispatchCancelled, dispatchInput>>
activityVars == <<activityEpoch, activeActivityEpochs,
                  settledActivities, nextActivityEpoch>>
protocolVars == <<requestDurable, admissionOutcome,
                  targetMessageCommitted, targetBoundaryCommitted,
                  sourceReceiptCommitted, receiptTiming, lastOutcome>>
vars == <<dispatchVars, activityVars, protocolVars>>

Ingress == INSTANCE RunIngressKernel WITH
    Owners <- Owners,
    NoOwner <- NoOwner,
    MaxEpoch <- MaxEpoch,
    kState <- dispatchState,
    kOwner <- dispatchOwner,
    kLeaseEpoch <- dispatchEpoch,
    kCancelRequested <- dispatchCancelled,
    kPendingInput <- dispatchInput

Activity == INSTANCE SessionActivityKernel WITH
    ActivityIds <- Activities,
    MaxActivityEpoch <- MaxEpoch,
    kActivityEpoch <- activityEpoch,
    kActiveActivityEpochs <- activeActivityEpochs,
    kSettledActivities <- settledActivities,
    kNextActivityEpoch <- nextActivityEpoch

Init ==
    /\ Ingress!Init
    /\ Activity!Init
    /\ requestDurable = FALSE
    /\ admissionOutcome = "None"
    /\ targetMessageCommitted = FALSE
    /\ targetBoundaryCommitted = FALSE
    /\ sourceReceiptCommitted = FALSE
    /\ receiptTiming = "None"
    /\ lastOutcome = "none"

PersistRequest ==
    /\ ~requestDurable
    /\ requestDurable' = TRUE
    /\ lastOutcome' = "request_committed"
    /\ UNCHANGED <<dispatchVars, activityVars, admissionOutcome,
                    targetMessageCommitted, targetBoundaryCommitted,
                    sourceReceiptCommitted, receiptTiming>>

OpenActivity ==
    /\ requestDurable
    /\ Activity!Open(MessageActivity)
    /\ lastOutcome' = "activity_opened"
    /\ UNCHANGED <<dispatchVars, requestDurable, admissionOutcome,
                    targetMessageCommitted, targetBoundaryCommitted,
                    sourceReceiptCommitted, receiptTiming>>

AdmitSuccess ==
    /\ admissionOutcome = "None"
    /\ activityEpoch[MessageActivity] \in activeActivityEpochs
    /\ Ingress!ActivateReservation
    /\ admissionOutcome' = "Success"
    /\ lastOutcome' = "admitted"
    /\ UNCHANGED <<activityVars, requestDurable,
                    targetMessageCommitted, targetBoundaryCommitted,
                    sourceReceiptCommitted, receiptTiming>>

\* Validation rejection proves that no dispatch committed, so the activity may
\* close. Identity conflict is modeled separately because prior work may exist.
AdmitRejected ==
    /\ admissionOutcome = "None"
    /\ activityEpoch[MessageActivity] \in activeActivityEpochs
    /\ Ingress!RejectReservation
    /\ Activity!Settle(MessageActivity)
    /\ admissionOutcome' = "Rejected"
    /\ lastOutcome' = "closed_rejection"
    /\ UNCHANGED <<requestDurable, targetMessageCommitted,
                    targetBoundaryCommitted, sourceReceiptCommitted,
                    receiptTiming>>

AdmitConflict ==
    /\ admissionOutcome = "None"
    /\ activityEpoch[MessageActivity] \in activeActivityEpochs
    /\ admissionOutcome' = "Conflict"
    /\ lastOutcome' = "identity_conflict"
    /\ UNCHANGED <<dispatchVars, activityVars, requestDurable,
                    targetMessageCommitted, targetBoundaryCommitted,
                    sourceReceiptCommitted, receiptTiming>>

\* An unavailable response represents both possible physical outcomes. Neither
\* branch settles the Session activity; exact retry is the only safe recovery.
AdmitUnknownBeforeCommit ==
    /\ admissionOutcome = "None"
    /\ activityEpoch[MessageActivity] \in activeActivityEpochs
    /\ dispatchState = "Reserved"
    /\ admissionOutcome' = "Unknown"
    /\ lastOutcome' = "unknown_before_commit"
    /\ UNCHANGED <<dispatchVars, activityVars, requestDurable,
                    targetMessageCommitted, targetBoundaryCommitted,
                    sourceReceiptCommitted, receiptTiming>>

AdmitUnknownAfterCommit ==
    /\ admissionOutcome = "None"
    /\ activityEpoch[MessageActivity] \in activeActivityEpochs
    /\ Ingress!ActivateReservation
    /\ admissionOutcome' = "Unknown"
    /\ lastOutcome' = "unknown_after_commit"
    /\ UNCHANGED <<activityVars, requestDurable,
                    targetMessageCommitted, targetBoundaryCommitted,
                    sourceReceiptCommitted, receiptTiming>>

RetryExact ==
    /\ admissionOutcome = "Unknown"
    /\ IF dispatchState = "Reserved"
          THEN /\ activityEpoch[MessageActivity] \in activeActivityEpochs
               /\ Ingress!ActivateReservation
          ELSE /\ dispatchState \in {"Pending", "Leased", "Awaiting", "Removed"}
               /\ UNCHANGED dispatchVars
    /\ admissionOutcome' = "Success"
    /\ lastOutcome' = "exact_retry"
    /\ UNCHANGED <<activityVars, requestDurable,
                    targetMessageCommitted, targetBoundaryCommitted,
                    sourceReceiptCommitted, receiptTiming>>

Claim(owner) ==
    /\ dispatchState = "Pending"
    /\ Ingress!Claim(owner)
    /\ lastOutcome' = "claimed"
    /\ UNCHANGED <<activityVars, requestDurable, admissionOutcome,
                    targetMessageCommitted, targetBoundaryCommitted,
                    sourceReceiptCommitted, receiptTiming>>

LeaseExpires(owner, epoch) ==
    /\ Ingress!Relinquish(owner, epoch)
    /\ lastOutcome' = "lease_expired"
    /\ UNCHANGED <<activityVars, requestDurable, admissionOutcome,
                    targetMessageCommitted, targetBoundaryCommitted,
                    sourceReceiptCommitted, receiptTiming>>

CommitTarget(owner, epoch) ==
    /\ dispatchState = "Leased"
    /\ dispatchOwner = owner
    /\ dispatchEpoch = epoch
    /\ ~targetMessageCommitted
    /\ targetMessageCommitted' = TRUE
    /\ lastOutcome' = "target_committed"
    /\ UNCHANGED <<dispatchVars, activityVars, requestDurable,
                    admissionOutcome, targetBoundaryCommitted,
                    sourceReceiptCommitted, receiptTiming>>

CommitTargetBoundary(owner, epoch) ==
    /\ dispatchState = "Leased"
    /\ dispatchOwner = owner
    /\ dispatchEpoch = epoch
    /\ targetMessageCommitted
    /\ ~targetBoundaryCommitted
    /\ targetBoundaryCommitted' = TRUE
    /\ lastOutcome' = "target_boundary_committed"
    /\ UNCHANGED <<dispatchVars, activityVars, requestDurable,
                    admissionOutcome, targetMessageCommitted,
                    sourceReceiptCommitted, receiptTiming>>

CommitSourceReceipt ==
    /\ admissionOutcome = "Success"
    /\ ~sourceReceiptCommitted
    /\ sourceReceiptCommitted' = TRUE
    /\ receiptTiming' = IF targetBoundaryCommitted
                          THEN "AfterTargetBoundary"
                          ELSE "BeforeTargetBoundary"
    /\ lastOutcome' = "source_receipt_committed"
    /\ UNCHANGED <<dispatchVars, activityVars, requestDurable,
                    admissionOutcome, targetMessageCommitted,
                    targetBoundaryCommitted>>

SettleTargetActivity ==
    /\ targetBoundaryCommitted
    /\ activityEpoch[MessageActivity] \in activeActivityEpochs
    /\ Activity!Settle(MessageActivity)
    /\ lastOutcome' = "target_activity_settled"
    /\ UNCHANGED <<dispatchVars, requestDurable, admissionOutcome,
                    targetMessageCommitted, targetBoundaryCommitted,
                    sourceReceiptCommitted, receiptTiming>>

SettleDispatch(owner, epoch) ==
    /\ targetBoundaryCommitted
    /\ activityEpoch[MessageActivity] \notin activeActivityEpochs
    /\ Ingress!SettleDone(owner, epoch)
    /\ lastOutcome' = "dispatch_settled"
    /\ UNCHANGED <<activityVars, requestDurable, admissionOutcome,
                    targetMessageCommitted, targetBoundaryCommitted,
                    sourceReceiptCommitted, receiptTiming>>

Next ==
    \/ PersistRequest
    \/ OpenActivity
    \/ AdmitSuccess
    \/ AdmitRejected
    \/ AdmitConflict
    \/ AdmitUnknownBeforeCommit
    \/ AdmitUnknownAfterCommit
    \/ RetryExact
    \/ \E owner \in Owners: Claim(owner)
    \/ \E owner \in Owners, epoch \in 1..MaxEpoch: LeaseExpires(owner, epoch)
    \/ \E owner \in Owners, epoch \in 1..MaxEpoch: CommitTarget(owner, epoch)
    \/ \E owner \in Owners, epoch \in 1..MaxEpoch:
           CommitTargetBoundary(owner, epoch)
    \/ CommitSourceReceipt
    \/ SettleTargetActivity
    \/ \E owner \in Owners, epoch \in 1..MaxEpoch: SettleDispatch(owner, epoch)

TypeOK ==
    /\ Ingress!TypeOK
    /\ Activity!TypeOK
    /\ requestDurable \in BOOLEAN
    /\ admissionOutcome \in {"None", "Success", "Rejected", "Conflict", "Unknown"}
    /\ targetMessageCommitted \in BOOLEAN
    /\ targetBoundaryCommitted \in BOOLEAN
    /\ sourceReceiptCommitted \in BOOLEAN
    /\ receiptTiming \in {"None", "BeforeTargetBoundary", "AfterTargetBoundary"}
    /\ lastOutcome \in {"none", "request_committed", "activity_opened",
                         "admitted", "closed_rejection", "identity_conflict",
                         "unknown_before_commit", "unknown_after_commit",
                         "exact_retry", "claimed", "lease_expired",
                         "target_committed", "target_boundary_committed",
                         "source_receipt_committed", "target_activity_settled",
                         "dispatch_settled"}

RequestBeforeActivity ==
    activityEpoch[MessageActivity] > 0 => requestDurable

ActivityBeforeDispatch ==
    dispatchState # "Reserved" => activityEpoch[MessageActivity] > 0

UnknownBeforeTargetBoundaryRetainsActivity ==
    admissionOutcome = "Unknown" /\ ~targetBoundaryCommitted
      => activityEpoch[MessageActivity] \in activeActivityEpochs

ConflictNeverSettlesPriorActivity ==
    admissionOutcome = "Conflict" /\ ~targetMessageCommitted
      => activityEpoch[MessageActivity] \in activeActivityEpochs

ReceiptRequiresDurableAdmission ==
    sourceReceiptCommitted => requestDurable /\ admissionOutcome = "Success"

ReceiptTimingIsExact ==
    /\ (receiptTiming = "None") = ~sourceReceiptCommitted
    /\ receiptTiming = "BeforeTargetBoundary" => sourceReceiptCommitted
    /\ receiptTiming = "AfterTargetBoundary" =>
          sourceReceiptCommitted /\ targetBoundaryCommitted

SourceReceiptCannotSettleActivityAlone ==
    sourceReceiptCommitted /\ ~targetBoundaryCommitted
      => activityEpoch[MessageActivity] \in activeActivityEpochs

TargetBoundaryRequiresCommittedMessage ==
    targetBoundaryCommitted => targetMessageCommitted

TargetSettlementPrecedesDispatchRemoval ==
    targetBoundaryCommitted /\ dispatchState = "Removed" =>
      activityEpoch[MessageActivity] \notin activeActivityEpochs

InternalMessageNeverUsesPendingInput == ~dispatchInput

Safety ==
    /\ TypeOK
    /\ Ingress!Safety
    /\ Activity!Safety
    /\ RequestBeforeActivity
    /\ ActivityBeforeDispatch
    /\ UnknownBeforeTargetBoundaryRetainsActivity
    /\ ConflictNeverSettlesPriorActivity
    /\ ReceiptRequiresDurableAdmission
    /\ ReceiptTimingIsExact
    /\ SourceReceiptCannotSettleActivityAlone
    /\ TargetBoundaryRequiresCommittedMessage
    /\ TargetSettlementPrecedesDispatchRemoval
    /\ InternalMessageNeverUsesPendingInput

Spec == Init /\ [][Next]_vars

\* Positive journey witnesses complement the full fault/interleaving safety
\* graph. Early receipt is the ordinary fast path. Late receipt models the
\* critical crash window where the child reaches a committed boundary and its
\* dispatch settles before the parent Runtime recovers the still-Executing
\* send_message call and commits the exact result.
CommitEarlySourceReceipt == ~targetMessageCommitted /\ CommitSourceReceipt
CommitLateSourceReceipt == dispatchState = "Removed" /\ CommitSourceReceipt

EarlyClaim(owner) == sourceReceiptCommitted /\ Claim(owner)

EarlyReceiptNext ==
    \/ PersistRequest
    \/ OpenActivity
    \/ AdmitSuccess
    \/ CommitEarlySourceReceipt
    \/ \E owner \in Owners: EarlyClaim(owner)
    \/ \E owner \in Owners, epoch \in 1..MaxEpoch: CommitTarget(owner, epoch)
    \/ \E owner \in Owners, epoch \in 1..MaxEpoch:
           CommitTargetBoundary(owner, epoch)
    \/ SettleTargetActivity
    \/ \E owner \in Owners, epoch \in 1..MaxEpoch: SettleDispatch(owner, epoch)

LateReceiptRecoveryNext ==
    \/ PersistRequest
    \/ OpenActivity
    \/ AdmitSuccess
    \/ \E owner \in Owners: Claim(owner)
    \/ \E owner \in Owners, epoch \in 1..MaxEpoch: CommitTarget(owner, epoch)
    \/ \E owner \in Owners, epoch \in 1..MaxEpoch:
           CommitTargetBoundary(owner, epoch)
    \/ SettleTargetActivity
    \/ \E owner \in Owners, epoch \in 1..MaxEpoch: SettleDispatch(owner, epoch)
    \/ CommitLateSourceReceipt

EarlyReceiptSpec ==
    Init /\ [][EarlyReceiptNext]_vars /\ WF_vars(EarlyReceiptNext)

LateReceiptRecoverySpec ==
    Init /\ [][LateReceiptRecoveryNext]_vars /\ WF_vars(LateReceiptRecoveryNext)

EarlyReceiptJourneyCompletes ==
    <> /\ dispatchState = "Removed"
       /\ sourceReceiptCommitted
       /\ targetBoundaryCommitted
       /\ receiptTiming = "BeforeTargetBoundary"

LateReceiptRecoveryCompletes ==
    <> /\ dispatchState = "Removed"
       /\ sourceReceiptCommitted
       /\ targetBoundaryCommitted
       /\ receiptTiming = "AfterTargetBoundary"
=============================================================================
