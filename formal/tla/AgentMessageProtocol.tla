------------------------- MODULE AgentMessageProtocol ----------------------
EXTENDS Naturals

\* Internal Agent messaging composes the canonical Session-activity and
\* Run-ingress kernels. This module owns only cross-domain ordering: committed
\* source request, admission certainty, target Thread commit, and source receipt.
CONSTANTS Owners, NoOwner, MessageActivity, MaxEpoch

Activities == {MessageActivity}

VARIABLES
    dispatchState, dispatchOwner, dispatchEpoch, dispatchCancelled, dispatchInput,
    activityEpoch, activeActivityEpochs, settledActivities, nextActivityEpoch,
    requestDurable, admissionOutcome, targetMessageCommitted,
    sourceReceiptCommitted, lastOutcome

dispatchVars == <<dispatchState, dispatchOwner, dispatchEpoch,
                  dispatchCancelled, dispatchInput>>
activityVars == <<activityEpoch, activeActivityEpochs,
                  settledActivities, nextActivityEpoch>>
protocolVars == <<requestDurable, admissionOutcome,
                  targetMessageCommitted, sourceReceiptCommitted, lastOutcome>>
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
    /\ sourceReceiptCommitted = FALSE
    /\ lastOutcome = "none"

PersistRequest ==
    /\ ~requestDurable
    /\ requestDurable' = TRUE
    /\ lastOutcome' = "request_committed"
    /\ UNCHANGED <<dispatchVars, activityVars, admissionOutcome,
                    targetMessageCommitted, sourceReceiptCommitted>>

OpenActivity ==
    /\ requestDurable
    /\ Activity!Open(MessageActivity)
    /\ lastOutcome' = "activity_opened"
    /\ UNCHANGED <<dispatchVars, requestDurable, admissionOutcome,
                    targetMessageCommitted, sourceReceiptCommitted>>

AdmitSuccess ==
    /\ activityEpoch[MessageActivity] \in activeActivityEpochs
    /\ Ingress!ActivateReservation
    /\ admissionOutcome' = "Success"
    /\ lastOutcome' = "admitted"
    /\ UNCHANGED <<activityVars, requestDurable,
                    targetMessageCommitted, sourceReceiptCommitted>>

\* Validation rejection proves that no dispatch committed, so the activity may
\* close. Identity conflict is modeled separately because prior work may exist.
AdmitRejected ==
    /\ activityEpoch[MessageActivity] \in activeActivityEpochs
    /\ Ingress!RejectReservation
    /\ Activity!Settle(MessageActivity)
    /\ admissionOutcome' = "Rejected"
    /\ lastOutcome' = "closed_rejection"
    /\ UNCHANGED <<requestDurable, targetMessageCommitted,
                    sourceReceiptCommitted>>

AdmitConflict ==
    /\ activityEpoch[MessageActivity] \in activeActivityEpochs
    /\ admissionOutcome' = "Conflict"
    /\ lastOutcome' = "identity_conflict"
    /\ UNCHANGED <<dispatchVars, activityVars, requestDurable,
                    targetMessageCommitted, sourceReceiptCommitted>>

\* An unavailable response represents both possible physical outcomes. Neither
\* branch settles the Session activity; exact retry is the only safe recovery.
AdmitUnknownBeforeCommit ==
    /\ activityEpoch[MessageActivity] \in activeActivityEpochs
    /\ dispatchState = "Reserved"
    /\ admissionOutcome' = "Unknown"
    /\ lastOutcome' = "unknown_before_commit"
    /\ UNCHANGED <<dispatchVars, activityVars, requestDurable,
                    targetMessageCommitted, sourceReceiptCommitted>>

AdmitUnknownAfterCommit ==
    /\ activityEpoch[MessageActivity] \in activeActivityEpochs
    /\ Ingress!ActivateReservation
    /\ admissionOutcome' = "Unknown"
    /\ lastOutcome' = "unknown_after_commit"
    /\ UNCHANGED <<activityVars, requestDurable,
                    targetMessageCommitted, sourceReceiptCommitted>>

RetryExact ==
    /\ admissionOutcome = "Unknown"
    /\ activityEpoch[MessageActivity] \in activeActivityEpochs
    /\ IF dispatchState = "Reserved"
          THEN Ingress!ActivateReservation
          ELSE /\ dispatchState \in {"Pending", "Leased"}
               /\ UNCHANGED dispatchVars
    /\ admissionOutcome' = "Success"
    /\ lastOutcome' = "exact_retry"
    /\ UNCHANGED <<activityVars, requestDurable,
                    targetMessageCommitted, sourceReceiptCommitted>>

Claim(owner) ==
    /\ dispatchState = "Pending"
    /\ Ingress!Claim(owner)
    /\ lastOutcome' = "claimed"
    /\ UNCHANGED <<activityVars, requestDurable, admissionOutcome,
                    targetMessageCommitted, sourceReceiptCommitted>>

LeaseExpires(owner, epoch) ==
    /\ Ingress!Relinquish(owner, epoch)
    /\ lastOutcome' = "lease_expired"
    /\ UNCHANGED <<activityVars, requestDurable, admissionOutcome,
                    targetMessageCommitted, sourceReceiptCommitted>>

CommitTarget(owner, epoch) ==
    /\ dispatchState = "Leased"
    /\ dispatchOwner = owner
    /\ dispatchEpoch = epoch
    /\ ~targetMessageCommitted
    /\ targetMessageCommitted' = TRUE
    /\ lastOutcome' = "target_committed"
    /\ UNCHANGED <<dispatchVars, activityVars, requestDurable,
                    admissionOutcome, sourceReceiptCommitted>>

CommitSourceReceipt ==
    /\ targetMessageCommitted
    /\ ~sourceReceiptCommitted
    /\ activityEpoch[MessageActivity] \in activeActivityEpochs
    /\ Activity!Settle(MessageActivity)
    /\ sourceReceiptCommitted' = TRUE
    /\ lastOutcome' = "source_receipt_committed"
    /\ UNCHANGED <<dispatchVars, requestDurable, admissionOutcome,
                    targetMessageCommitted>>

SettleDispatch(owner, epoch) ==
    /\ targetMessageCommitted
    /\ Ingress!SettleDone(owner, epoch)
    /\ lastOutcome' = "dispatch_settled"
    /\ UNCHANGED <<activityVars, requestDurable, admissionOutcome,
                    targetMessageCommitted, sourceReceiptCommitted>>

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
    \/ CommitSourceReceipt
    \/ \E owner \in Owners, epoch \in 1..MaxEpoch: SettleDispatch(owner, epoch)

TypeOK ==
    /\ Ingress!TypeOK
    /\ Activity!TypeOK
    /\ requestDurable \in BOOLEAN
    /\ admissionOutcome \in {"None", "Success", "Rejected", "Conflict", "Unknown"}
    /\ targetMessageCommitted \in BOOLEAN
    /\ sourceReceiptCommitted \in BOOLEAN
    /\ lastOutcome \in {"none", "request_committed", "activity_opened",
                         "admitted", "closed_rejection", "identity_conflict",
                         "unknown_before_commit", "unknown_after_commit",
                         "exact_retry", "claimed", "lease_expired",
                         "target_committed", "source_receipt_committed",
                         "dispatch_settled"}

RequestBeforeActivity ==
    activityEpoch[MessageActivity] > 0 => requestDurable

ActivityBeforeDispatch ==
    dispatchState # "Reserved" => activityEpoch[MessageActivity] > 0

UnknownNeverSettlesActivity ==
    admissionOutcome = "Unknown" /\ ~targetMessageCommitted
      => activityEpoch[MessageActivity] \in activeActivityEpochs

ConflictNeverSettlesPriorActivity ==
    admissionOutcome = "Conflict" /\ ~targetMessageCommitted
      => activityEpoch[MessageActivity] \in activeActivityEpochs

ReceiptRequiresTargetCommit == sourceReceiptCommitted => targetMessageCommitted

ReceiptSettlesActivity ==
    sourceReceiptCommitted
      => activityEpoch[MessageActivity] \notin activeActivityEpochs

InternalMessageNeverUsesPendingInput == ~dispatchInput

Safety ==
    /\ TypeOK
    /\ Ingress!Safety
    /\ Activity!Safety
    /\ RequestBeforeActivity
    /\ ActivityBeforeDispatch
    /\ UnknownNeverSettlesActivity
    /\ ConflictNeverSettlesPriorActivity
    /\ ReceiptRequiresTargetCommit
    /\ ReceiptSettlesActivity
    /\ InternalMessageNeverUsesPendingInput

Spec == Init /\ [][Next]_vars
=============================================================================
